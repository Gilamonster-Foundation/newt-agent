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
