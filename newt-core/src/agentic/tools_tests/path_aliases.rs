//! Real filesystem regressions grounding the lexical permission-gate fixtures.
use super::*;
use crate::{Caveats, Scope};

#[tokio::test]
async fn path_alias_grants_match_without_authorizing_siblings_or_escapes() {
    use crate::agentic::permissions::{widen_caveats, DenialKind};
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    for (grant, request) in [(&real, &alias), (&alias, &real)] {
        let base = Caveats {
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let authority = widen_caveats(
            &base,
            &[(DenialKind::FsWrite, grant.to_str().unwrap().into())],
        );
        let file = request.join("new/file.txt");
        assert!(tui_permits_path(
            &authority.fs_write,
            file.to_str().unwrap()
        ));
        let Some(Some((root, relative))) =
            object_bound_target(&authority.fs_write, file.to_str().unwrap())
        else {
            panic!("alias lost at object binding")
        };
        let mut handle =
            crate::fs_cap::WorkspaceDir::create_granted_file(std::path::Path::new(root), &relative)
                .unwrap();
        std::io::Write::write_all(&mut handle, b"persisted").unwrap();
        assert_eq!(
            std::fs::read(real.join("new/file.txt")).unwrap(),
            b"persisted"
        );
        assert!(!tui_permits_path(
            &authority.fs_write,
            temp.path().join("real-sibling/file").to_str().unwrap()
        ));
        std::os::unix::fs::symlink(temp.path(), real.join("escape")).ok();
        assert!(!tui_permits_path(
            &authority.fs_write,
            request.join("escape/outside").to_str().unwrap()
        ));
    }
}

/// macOS /tmp is a host alias, not the per-child disposable TMPDIR. Ground
/// permission matching in actual adoption, file dispatch, and confined writes.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "native Seatbelt and Git persistence proof"]
async fn mac_tmp_alias_dispatch_and_adoption_persist() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _bypass = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = EnvVar::unset("NEWT_FULL_ACCESS");
    assert!(crate::confined_exec::kernel_fs_fence_available());
    for alias in ["/tmp", "/var", "/etc"] {
        assert_eq!(
            crate::caveats::canonical_fs_path(alias).unwrap(),
            std::path::PathBuf::from(format!("/private{alias}"))
        );
    }
    for reverse in [false, true] {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = temp.path().join("main");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        ] {
            let out = super::git_fixture::hermetic_git(&root, temp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let real = temp.path().join("task");
        let alias = std::path::Path::new("/tmp")
            .join(temp.path().file_name().unwrap())
            .join("task");
        let (grant, request) = if reverse {
            (&alias, &real)
        } else {
            (&real, &alias)
        };
        let base = Caveats {
            fs_write: Scope::only([root.to_str().unwrap().into()]),
            ..Caveats::top()
        };
        let authority = crate::widen_caveats(
            &base,
            &[(DenialKind::FsWrite, grant.to_str().unwrap().into())],
        );
        let session = crate::worktree_adoption::WorktreeSession::default();
        let mut presentation = NullPresentation;
        for (name, args) in [
            (
                "run_command",
                serde_json::json!({"command":format!("git worktree add -b task '{}'", request.display()), "env": super::git_fixture::hermetic_git_env(temp.path())}),
            ),
            (
                "write_file",
                serde_json::json!({"path":request.join("file.txt"), "content":"file persists"}),
            ),
            (
                "run_command",
                serde_json::json!({"command":format!("printf shell-persists > '{}'", request.join("shell.txt").display()), "cwd":request}),
            ),
        ] {
            let outcome = std::sync::OnceLock::new();
            let out = worktree::execute(
                &mut presentation,
                name,
                &args,
                root.to_str().unwrap(),
                false,
                100,
                &authority,
                &mut crate::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(&session),
                    execution: Some(&outcome),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await;
            eprintln!("alias direction {reverse}, {name}: {out}");
            if name == "run_command" {
                assert_eq!(outcome.get(), Some(&crate::ExecOutcome::Passed), "{out}");
            }
            assert!(
                !out.contains("capability denied") && !out.contains("error:"),
                "{name}: {out}"
            );
            if name == "write_file" {
                assert_eq!(
                    std::fs::read_to_string(real.join("file.txt")).unwrap(),
                    "file persists"
                );
            }
        }
        assert!(session.snapshot().is_some(), "creation must actually adopt");
        let denied = worktree::execute(
            &mut presentation,
            "write_file",
            &serde_json::json!({"path":root.join("forbidden.txt"), "content":"no"}),
            root.to_str().unwrap(),
            false,
            100,
            &authority,
            &mut crate::NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
        )
        .await;
        assert!(denied.contains("capability denied"), "{denied}");
        assert!(!root.join("forbidden.txt").exists());
        assert_eq!(
            std::fs::read_to_string(real.join("shell.txt")).unwrap(),
            "shell-persists"
        );
        let out = super::git_fixture::hermetic_git(&root, temp.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(real.to_str().unwrap()));
    }
}

#[cfg(target_os = "macos")]
struct NullPresentation;
#[cfg(target_os = "macos")]
impl ToolPresentation for NullPresentation {
    fn preview(&mut self, _: &str, _: usize) {}
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

/// An approved alias names its target at approval, not whatever it is changed
/// to later; missing leaves resolve without creating them, dangling links fail.
#[test]
fn path_alias_grant_is_pinned_and_missing_leaf_is_not_created() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let first = root.join("first");
    let second = root.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let missing = alias.join("missing/leaf");
    assert_eq!(
        crate::config::resolve_uncreated_path(&missing).unwrap(),
        first.join("missing/leaf")
    );
    assert!(!first.join("missing").exists());
    let authority = crate::widen_caveats(
        &Caveats {
            fs_write: Scope::none(),
            ..Caveats::top()
        },
        &[(DenialKind::FsWrite, alias.to_str().unwrap().into())],
    );
    assert_eq!(
        authority.fs_write,
        Scope::only([first.to_str().unwrap().to_owned()])
    );
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&second, &alias).unwrap();
    assert!(!tui_permits_path(
        &authority.fs_write,
        alias.join("new").to_str().unwrap()
    ));
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(root.join("absent"), &alias).unwrap();
    assert!(crate::config::resolve_uncreated_path(&alias.join("leaf")).is_err());
}

/// CLI and durable decisions must agree with transient grants, including the
/// more restrictive durable verdict when two spellings name the same object.
#[test]
fn path_alias_cli_and_durable_verdicts_share_canonicalization() {
    use crate::ocap_store::{build_store, evaluate_request, normalize_fs_path, Verdict};
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let alias = alias.to_str().unwrap();
    let real = real.to_str().unwrap();
    let mut authority = Caveats::top();
    crate::caveats::lock_fs_to_workspace(&mut authority, alias, &[], &[]);
    assert_eq!(authority.fs_write, Scope::only([real.to_owned()]));
    assert_eq!(normalize_fs_path(alias).unwrap(), real);
    let (policy, errors) = build_store(&[
        (
            Verdict::Approve,
            Some(format!("[[fs]]\npath = {real:?}\nwrite = true\n")),
        ),
        (Verdict::Deny, Some(format!("[[fs]]\npath = {alias:?}\n"))),
    ]);
    assert!(errors.is_empty(), "{errors:?}");
    for path in [alias, real] {
        for kind in [DenialKind::FsRead, DenialKind::FsWrite] {
            assert_eq!(evaluate_request(&policy, kind, path), Some(Verdict::Deny));
        }
    }
}

/// Alias normalization must not turn unlink(alias/inside-link) into unlink of
/// the link's target; the below-root descriptor walk owns symlink semantics.
#[test]
fn path_alias_object_binding_preserves_below_root_components() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("target"), "keep").unwrap();
    std::os::unix::fs::symlink("target", real.join("link")).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let scope = Scope::only([real.to_str().unwrap().into()]);
    let Some(Some((_, relative))) =
        object_bound_target(&scope, alias.join("link").to_str().unwrap())
    else {
        panic!("alias should match")
    };
    assert_eq!(relative, std::path::Path::new("link"));
}
