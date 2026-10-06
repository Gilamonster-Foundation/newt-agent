//! Select governed publication before any broker construction or preflight.
use crate::agentic::display::ToolPresentation;
use std::sync::Once;

const BYPASS_NOTICE: &str = "governed publication bypassed (OCAP disabled)";

pub(in crate::agentic::tools) fn governed(
    source: &str,
    bypass: bool,
    presentation: &mut dyn ToolPresentation,
) -> bool {
    // Launch authority is process-scoped and frozen; disclose its publication
    // consequence once per process, independently of tool-output folding.
    static NOTICE: Once = Once::new();
    route(source, bypass, &NOTICE, presentation)
}

fn route(
    source: &str,
    bypass: bool,
    notice: &Once,
    presentation: &mut dyn ToolPresentation,
) -> bool {
    if bypass
        && (super::needs_commit_broker(source)
            || super::needs_push_broker(source)
            || super::needs_pr_create_broker(source))
    {
        notice.call_once(|| presentation.preview(BYPASS_NOTICE, 0));
    }
    !bypass
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Notices(Vec<String>);
    impl ToolPresentation for Notices {
        fn preview(&mut self, text: &str, _: usize) {
            self.0.push(text.into());
        }
        fn document(&mut self, _: &str) {}
        fn override_result(&mut self, _: String) {}
    }

    /// #2765: portable route selection skips all publication brokers only in
    /// explicit bypass mode, and emits one notice across commit, push and PR.
    #[test]
    fn publication_2765_routing_and_notice_are_portable() {
        let once = Once::new();
        let mut display = Notices::default();
        assert!(!route("git status", true, &once, &mut display));
        assert!(display.0.is_empty());
        for source in [
            "git commit -m fix",
            "git push -u origin HEAD",
            "gh pr create --title fix --body fix",
            "git.exe commit -m fix",
            "git.exe push origin HEAD",
            "gh.exe pr create --title fix",
            "git add file && git commit -m fix && git push",
        ] {
            assert!(route(source, false, &Once::new(), &mut Notices::default()));
            assert!(!route(source, true, &once, &mut display), "{source}");
        }
        assert_eq!(display.0, [BYPASS_NOTICE]);
    }

    /// #2765: ground the portable decision in actual host Git commit and push
    /// to a local bare remote. No signing policy/broker is installed. Also
    /// prove PR-create dispatch with a local gh stub, never a forge request.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "real Git and host shell; selected explicitly by Linux CI"]
    async fn publication_2765_plain_git_dispatch() {
        use crate::agentic::tools::tests::git_shell_grant::hermetic_git;
        use crate::agentic::tools::{
            disable_ocap_tests::{env_lock, EnvVar},
            ToolCollaborators,
        };
        use std::os::unix::fs::PermissionsExt;
        let _lock = env_lock().await;
        let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let _full = EnvVar::set("NEWT_FULL_ACCESS", "1");
        let _venv = EnvVar::unset("NEWT_VENV");
        let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let bin = temp.path().join("bin");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf '%s\\n' \"$*\" > gh-args\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _paths = EnvVar::set("NEWT_EXEC_PATHS", bin.to_str().unwrap());
        for args in [
            vec!["init", "-q", "-b", "topic"],
            vec!["init", "--bare", "-q", remote.to_str().unwrap()],
        ] {
            assert!(hermetic_git(&root, temp.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let _git_env = [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_AUTHOR_NAME", "fixture"),
            ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
            ("GIT_COMMITTER_NAME", "fixture"),
            ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
        ]
        .map(|(k, v)| EnvVar::set(k, v));
        let _git_redirects = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_NAMESPACE",
        ]
        .map(EnvVar::unset);
        let git = "/usr/bin/git";
        let commands = [
            format!("{git} -c commit.gpgsign=false commit --allow-empty -m fixture"),
            format!("{git} push '{}' HEAD:refs/heads/topic", remote.display()),
            "gh pr create --title fixture --body fixture".into(),
        ];
        let mut display = Notices::default();
        for command in commands {
            let outcome = std::sync::OnceLock::new();
            let receipt = std::sync::OnceLock::new();
            let text = crate::agentic::tools::execute_tool_inner(
                &mut display,
                "run_command",
                &serde_json::json!({"command":command}),
                root.to_str().unwrap(),
                false,
                40,
                &crate::Caveats::top(),
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    execution: Some(&outcome),
                    governed_pr: Some(&receipt),
                    ..Default::default()
                },
                false,
                crate::agentic::PromptDisposition::Act,
            )
            .await;
            assert_eq!(
                outcome.get(),
                Some(&crate::ExecOutcome::Passed),
                "{command}: {text}"
            );
            assert!(
                !text.contains("refused") && !text.contains("capability denied"),
                "{text}"
            );
            assert!(
                receipt.get().is_none(),
                "plain gh must not mint a governed receipt"
            );
        }
        let oid = |path: &std::path::Path| {
            let out = hermetic_git(path, temp.path())
                .args(["rev-parse", "refs/heads/topic"])
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        };
        assert_eq!(oid(&root), oid(&remote));
        assert_eq!(
            std::fs::read_to_string(root.join("gh-args")).unwrap(),
            "pr create --title fixture --body fixture\n"
        );
        assert_eq!(
            display
                .0
                .iter()
                .filter(|s| s.as_str() == BYPASS_NOTICE)
                .count(),
            1
        );
    }
}
