//! Deterministic real-root replacement at an existing Git await boundary.
//! PATH instrumentation is isolated in one exact-test subprocess.
use super::git_safety_tests::{commit, git};
use super::*;
use crate::Scope;
use std::path::Path;

#[tokio::test(flavor = "current_thread")]
async fn review_capture_root_replacement_cannot_mix_retained_and_pathname_trees() {
    const CHILD: &str = "NEWT_REVIEW_ROOT_REPLACEMENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_review::root_identity_tests::review_capture_root_replacement_cannot_mix_retained_and_pathname_trees", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "isolated root capture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _environment = crate::process_env::lock();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let replacement = scratch.path().join("replacement");
    let parked = scratch.path().join("original-directory");
    for (root, label) in [(&workspace, "A"), (&replacement, "B")] {
        std::fs::create_dir(root).unwrap();
        git(root, &["init", "-q"]);
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("normal.txt"), format!("normal {label}")).unwrap();
        std::fs::write(
            root.join("target/source.txt"),
            format!("tracked target {label}"),
        )
        .unwrap();
        git(root, &["add", "--", "normal.txt", "target/source.txt"]);
        commit(root);
    }
    let original = capture_workspace(&workspace, &Scope::All, 65536)
        .await
        .unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let program = std::env::split_paths(&original_path)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let tools = scratch.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let marker = scratch.path().join("replaced");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = format!(
        "#!/bin/sh\nif [ ! -e {} ]; then\n mv {} {} || exit 91\n mv {} {} || exit 92\n printf replaced > {}\nfi\nexec {} \"$@\"\n",
        quote(&marker),
        quote(&workspace),
        quote(&parked),
        quote(&replacement),
        quote(&workspace),
        quote(&marker),
        quote(&program)
    );
    let shim = tools.join("git");
    std::fs::write(&shim, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut search = vec![tools];
    search.extend(std::env::split_paths(&original_path));
    crate::process_env::set_var(
        "PATH",
        &std::env::join_paths(search).unwrap().to_string_lossy(),
    );
    let result = capture_workspace(&workspace, &Scope::All, 65536).await;
    crate::process_env::set_var("PATH", &original_path.to_string_lossy());
    assert!(marker.exists(), "replacement boundary was not reached");
    match result {
        Err(CaptureFailure::Incomplete(_)) => {}
        Ok(actual) => assert_eq!(
            actual, original,
            "complete capture mixed normal A with tracked override B"
        ),
        Err(other) => panic!("unexpected capture disposition: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn review_git_root_replacement_cannot_mix_metadata_and_working_versions() {
    use content_addressable::ContentAddressable;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    const CHILD: &str = "NEWT_REVIEW_GIT_ROOT_REPLACEMENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_review::root_identity_tests::review_git_root_replacement_cannot_mix_metadata_and_working_versions", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "isolated Git root capture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _environment = crate::process_env::lock();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let replacement = scratch.path().join("replacement");
    let parked = scratch.path().join("original-directory");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    std::fs::write(workspace.join("tracked.txt"), "common HEAD").unwrap();
    git(&workspace, &["add", "tracked.txt"]);
    commit(&workspace);
    std::fs::write(workspace.join("tracked.txt"), "staged A").unwrap();
    git(&workspace, &["add", "tracked.txt"]);
    std::fs::write(workspace.join("tracked.txt"), "working A").unwrap();
    let staged = String::from_utf8(git(&workspace, &["rev-parse", ":tracked.txt"])).unwrap();
    let staged = staged.trim();
    git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--no-hardlinks",
            workspace.to_str().unwrap(),
            replacement.to_str().unwrap(),
        ],
    );
    // The related repository contains A's staged object, but its own index and
    // working bytes differ. A mixed subject is therefore internally plausible.
    assert_eq!(
        git(&replacement, &["cat-file", "blob", staged]),
        b"staged A"
    );
    std::fs::write(replacement.join("tracked.txt"), "staged B").unwrap();
    git(&replacement, &["add", "tracked.txt"]);
    std::fs::write(replacement.join("tracked.txt"), "working B").unwrap();
    let objective = ReviewObjective {
        instruction: "review existing work".into(),
        turn_context: "root binding fixture".into(),
    }
    .content_id()
    .unwrap();
    let original = capture_existing_diff(
        &workspace,
        &Scope::All,
        objective,
        65536,
        65536,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let program = std::env::split_paths(&original_path)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let tools = scratch.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let marker = scratch.path().join("replacements");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    // Restore A before each metadata pass, replace it with B at the final
    // staged-blob read, then let production read the working pathname. This
    // repeats the same mixed subject and defeats two-pass equality alone.
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" ls-tree "*)
    if [ -d {parked} ]; then
      mv {workspace} {replacement} || exit 91
      mv {parked} {workspace} || exit 92
    fi
    cd {workspace} || exit 95
    ;;
  *" cat-file blob {staged} "*)
    if [ ! -d {parked} ]; then
      mv {workspace} {parked} || exit 93
      mv {replacement} {workspace} || exit 94
      printf replaced\\n >> {marker}
    fi
    ;;
esac
exec {program} "$@"
"#,
        parked = quote(&parked),
        workspace = quote(&workspace),
        replacement = quote(&replacement),
        marker = quote(&marker),
        program = quote(&program)
    );
    let shim = tools.join("git");
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut search = vec![tools];
    search.extend(std::env::split_paths(&original_path));
    crate::process_env::set_var(
        "PATH",
        &std::env::join_paths(search).unwrap().to_string_lossy(),
    );
    let result = capture_existing_diff(
        &workspace,
        &Scope::All,
        objective,
        65536,
        65536,
        Duration::from_secs(10),
    )
    .await;
    crate::process_env::set_var("PATH", &original_path.to_string_lossy());
    assert!(marker.exists(), "replacement boundary was not reached");
    match result {
        Err(CaptureFailure::Incomplete(_)) => {}
        Ok(actual) => assert_eq!(
            actual.content_id().unwrap(),
            original.content_id().unwrap(),
            "complete subject mixed staged A with working B: {}",
            actual.material().unwrap()
        ),
        Err(other) => panic!("unexpected capture disposition: {other:?}"),
    }
}
