/// The fixture git, by absolute path: an inherited `PATH` cannot substitute
/// another binary, and it is the same root-owned `/usr/bin/git` the #2630
/// exec-path alias is proven against. The kernel-fence tests skip without
/// it; the rest fail loudly ([`hermetic_git`]).
#[cfg(unix)]
const FIXTURE_GIT: &str = "/usr/bin/git";

/// The explicit, minimal environment every fixture git runs under, for BOTH
/// the unconfined setup ([`hermetic_git`]) and the confined dispatch (the
/// `"env"` seam of `dispatch_bridled_shell`): one definition, so "isolated
/// enough not to touch a real repository" and "isolated enough to be a fair
/// confinement proof" cannot drift apart. A private `HOME` plus
/// `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL`/`GIT_TEMPLATE_DIR` close the
/// config and template sources that live outside the process environment
/// (`/etc/gitconfig`, the operator's `~/.gitconfig`, `~/.config/git`).
pub(in crate::agentic::tools) fn hermetic_git_env(
    home: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let home = home.to_string_lossy();
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    [
        ("HOME", home.as_ref()),
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.invalid"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.invalid"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null),
        ("GIT_TEMPLATE_DIR", null),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Mirror of the vendored core's test-private `hermetic_git_command`
/// (`vendor/agent-bridle-core/src/sandbox.rs`, bridle PR #407): `env_clear()`
/// rather than a denylist, so an inherited `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_CEILING_DIRECTORIES`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
/// `GIT_OBJECT_DIRECTORY` or any other ambient git knob cannot leak in,
/// because nothing is inherited. Proven by
/// `hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture`.
/// Widened to `pub(in crate::agentic::tools)` (#2681 round 3) so the
/// `execute_tool_branch_tests::permissions` git-broker fixture reuses it
/// too, rather than a second ad hoc git fixture that inherits the ambient
/// `GIT_DIR`/`HOME`/hooks.
pub(in crate::agentic) fn hermetic_git(
    dir: &std::path::Path,
    home: &std::path::Path,
) -> std::process::Command {
    #[cfg(unix)]
    assert!(
        std::path::Path::new(FIXTURE_GIT).exists(),
        "the fixture git {FIXTURE_GIT} is absent"
    );
    #[cfg(unix)]
    let program = std::path::PathBuf::from(FIXTURE_GIT);
    #[cfg(windows)]
    let program =
        crate::git_hardening::trusted_git_program(std::env::var_os("PATH").as_deref()).unwrap();
    let mut cmd = std::process::Command::new(program);
    cmd.current_dir(dir)
        .env_clear()
        .envs(hermetic_git_env(home));
    #[cfg(windows)]
    for key in ["SystemRoot", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

/// #3315: fixture authority is limited to the trees a test supplies. The real
/// worktree/broker cases ground this pure scope contract under confinement.
pub(crate) fn read_caveats(roots: &[&std::path::Path]) -> crate::Caveats {
    crate::Caveats {
        fs_read: crate::Scope::only(roots.iter().map(|root| root.to_string_lossy().into_owned())),
        ..crate::Caveats::top()
    }
}

// Invocation-local filesystem view for tests of unrestricted authority. The
// scope remains All; only these fixture objects exist at the read-binding seam.
tokio::task_local! {
    pub(crate) static READ_VIEW: crate::Scope<String>;
}

#[test]
fn fixture_read_authority_excludes_unrelated_trees() {
    use crate::{Caveats, Scope};
    let root = std::path::Path::new("fixture");
    let other = std::path::Path::new("other");
    let caveats = read_caveats(&[root]);
    assert!(newt_policy::caveats::permits_path(
        &caveats.fs_read,
        "fixture/repo/HEAD"
    ));
    assert!(!newt_policy::caveats::permits_path(
        &caveats.fs_read,
        "other/secret"
    ));
    assert_eq!(read_caveats(&[]).fs_read, Scope::none());
    assert_eq!(
        read_caveats(&[root, other]).fs_read,
        Scope::only(["fixture".into(), "other".into()])
    );
    assert_eq!(
        Caveats {
            fs_read: Scope::All,
            ..caveats
        },
        Caveats::top()
    );
}

/// Grounds the pure fixture scope contract in real held descriptors. Full-read
/// policy remains All while the injected view contains only the supplied tree.
#[cfg(unix)]
#[test]
fn unrestricted_read_view_keeps_all_semantics_and_fixture_handles() {
    let fixture = tempfile::tempdir().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("marker"), "inside").unwrap();
    std::fs::write(unrelated.path().join("marker"), "outside").unwrap();
    let view = read_caveats(&[fixture.path()]).fs_read;
    assert!(READ_VIEW.try_with(|_| ()).is_err());
    READ_VIEW.sync_scope(view.clone(), || {
        let held = crate::git_staging::HeldRoots::bind(&crate::Scope::All).unwrap();
        assert_eq!(held.scope(), &crate::Scope::All);
        assert_eq!(held.canonical(), &[fixture.path().canonicalize().unwrap()]);
        held.verify_identities().unwrap();
        let marker = std::path::Path::new("marker");
        assert_eq!(
            held.read_to_string(fixture.path(), marker)
                .unwrap()
                .as_deref(),
            Some("inside")
        );
        assert!(held.read_to_string(unrelated.path(), marker).is_err());
        let denied = crate::git_staging::HeldRoots::bind(&crate::Scope::none()).unwrap();
        assert!(denied.canonical().is_empty());
        READ_VIEW.sync_scope(crate::Scope::none(), || {
            assert!(crate::git_staging::HeldRoots::bind(&crate::Scope::All)
                .unwrap()
                .canonical()
                .is_empty());
        });
        assert_eq!(READ_VIEW.with(Clone::clone), view);
    });
    assert!(READ_VIEW.try_with(|_| ()).is_err());
}
