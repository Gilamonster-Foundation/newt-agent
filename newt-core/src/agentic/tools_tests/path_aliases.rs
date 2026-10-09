use super::*;
use crate::{Caveats, Scope};

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
            crate::caveats::lexically_normalize(alias),
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

/// Legacy system spellings retain verdict precedence and read-only semantics;
/// neither suffix normalization nor an arbitrary alias can extend a signature.
#[test]
fn mac_durable_fixed_aliases_keep_precedence_and_modes() {
    use crate::ocap_store::{build_store, evaluate_request, Verdict};
    for (stored, request) in [
        ("/tmp/legacy", "/private/tmp/legacy"),
        ("/private/tmp/legacy", "/tmp/legacy"),
    ] {
        let policy =
            |path: &str, write: bool| Some(format!("[[fs]]\npath = {path:?}\nwrite = {write}\n"));
        let (set, warnings) = build_store(&[(Verdict::Approve, policy(stored, false))]);
        assert!(warnings.is_empty());
        assert_eq!(
            evaluate_request(&set, DenialKind::FsRead, request),
            Some(Verdict::Approve)
        );
        assert_eq!(evaluate_request(&set, DenialKind::FsWrite, request), None);
        assert_eq!(
            evaluate_request(&set, DenialKind::FsRead, &format!("{request}/../other")),
            None
        );
        for verdict in [Verdict::Ask, Verdict::Passkey, Verdict::Deny] {
            let (set, _) = build_store(&[
                (Verdict::Approve, policy(stored, true)),
                (verdict, policy(request, false)),
            ]);
            for kind in [DenialKind::FsRead, DenialKind::FsWrite] {
                assert_eq!(evaluate_request(&set, kind, request), Some(verdict));
                assert_eq!(evaluate_request(&set, kind, stored), Some(verdict));
            }
        }
    }
}
