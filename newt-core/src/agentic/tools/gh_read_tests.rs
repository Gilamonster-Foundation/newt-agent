//! In-memory acceptance through the production run_command dispatcher.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Probe {
    pub(super) host: AtomicUsize,
    pub(super) confined: AtomicUsize,
    pub(super) remote: String,
    pub(super) calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    pub(super) output: String,
    pub(super) exit_code: i32,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            host: AtomicUsize::new(0),
            confined: AtomicUsize::new(0),
            remote: "github.com/owner/repo".into(),
            calls: Default::default(),
            output: "issue 1".into(),
            exit_code: 0,
        }
    }
}
impl Probe {
    pub(in crate::agentic::tools) fn run(
        &self,
        host: &str,
        argv: &[String],
    ) -> Result<native_git::gh_read::ReadOutcome, String> {
        self.host.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push((host.into(), argv.to_vec()));
        Ok(native_git::gh_read::ReadOutcome {
            stdout: self.output.clone(),
            code: Some(self.exit_code),
        })
    }
}

#[derive(Default)]
struct Gate(Vec<PermissionRequest>);
impl PermissionGate for Gate {
    fn ask_dependency_fetch(
        &mut self,
        _prepared: &newt_policy::caveats::Caveats,
        _request: &newt_policy::permissions::PermissionRequest,
    ) -> newt_policy::permissions::PermissionDecision {
        newt_policy::permissions::PermissionDecision::Deny
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        panic!("read broker must not ask a freeform question")
    }
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        self.0.extend_from_slice(requests);
        PermissionDecision::Deny
    }
}

/// #3089: launch grants must reach a host-enforcing read broker, not the
/// confined child whose network scope is necessarily deny-all. Resources at
/// the dispatch boundaries are in-memory; no directory, executable or server
/// is consulted. The ignored real broker ground verifies these stand-ins.
#[tokio::test]
async fn launch_granted_gh_issue_view_uses_host_broker_without_prompts() {
    let _env = crate::process_env::lock();
    let probe = Probe::default();
    let mut gate = Gate::default();
    let caveats = crate::Caveats {
        net: crate::Scope::only(["github.com".into(), "api.github.com".into()]),
        fs_write: crate::Scope::only(["/virtual-workspace".into()]),
        ..crate::Caveats::top()
    };
    let mut display = ToolDisplay::new(Vec::<u8>::new(), false, 80, 20, false);
    let result = execute_tool_inner(
        &mut display,
        "run_command",
        &serde_json::json!({"command": "gh issue view 1 -R owner/repo"}),
        "/virtual-workspace",
        false,
        20,
        &caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            command_probe: Some(&probe),
            permission_gate: Some(&mut gate),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert_eq!(
        probe.host.load(Ordering::SeqCst),
        1,
        "launch-granted gh issue view did not reach the host broker: {result}"
    );
    assert_eq!(probe.confined.load(Ordering::SeqCst), 0);
    assert!(
        gate.0.is_empty(),
        "launch grants must avoid permission prompts"
    );
    assert!(result.contains("issue 1"), "{result}");
}

async fn dispatch(
    command: &str,
    caveats: &crate::Caveats,
    probe: &Probe,
    gate: &mut dyn PermissionGate,
) -> String {
    let _env = crate::process_env::lock();
    let mut display = ToolDisplay::new(Vec::<u8>::new(), false, 80, 20, false);
    execute_tool_inner(
        &mut display,
        "run_command",
        &serde_json::json!({"command": command}),
        "/virtual-workspace",
        false,
        20,
        caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            command_probe: Some(probe),
            permission_gate: Some(gate),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await
}
fn granted() -> crate::Caveats {
    crate::Caveats {
        net: crate::Scope::only(["github.com".into(), "api.github.com".into()]),
        fs_write: crate::Scope::only(["/virtual-workspace".into()]),
        ..crate::Caveats::top()
    }
}

#[tokio::test]
async fn gh_read_allowlist_rebuilds_argv_and_pins_the_repository() {
    for (command, expected) in [
        (
            "gh issue view 1 -R owner/repo --comments",
            vec!["issue", "view", "1", "--comments"],
        ),
        (
            "gh issue list --limit=7 --state closed",
            vec!["issue", "list", "--limit", "7", "--state", "closed"],
        ),
        (
            "gh issue status --json title,number",
            vec!["issue", "status", "--json", "title,number"],
        ),
        ("gh pr view 1 -c", vec!["pr", "view", "1", "--comments"]),
        (
            "gh pr list -L 4 -s merged",
            vec!["pr", "list", "--limit", "4", "--state", "merged"],
        ),
        (
            "gh pr status --conflict-status",
            vec!["pr", "status", "--conflict-status"],
        ),
        (
            "gh pr diff 1 --name-only --patch",
            vec!["pr", "diff", "1", "--name-only", "--patch"],
        ),
        (
            "gh pr checks 1 --required",
            vec!["pr", "checks", "1", "--required"],
        ),
        (
            "/untrusted/gh issue view 1 2>&1 | head -1; echo done",
            vec!["issue", "view", "1"],
        ),
    ] {
        let probe = Probe::default();
        let mut gate = Gate::default();
        let out = dispatch(command, &granted(), &probe, &mut gate).await;
        assert!(out.contains("issue 1"), "{command}: {out}");
        assert!(gate.0.is_empty());
        let calls = probe.calls.lock().unwrap();
        let mut expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        expected.extend(["--repo".into(), "github.com/owner/repo".into()]);
        assert_eq!(
            calls.as_slice(),
            &[("github.com".into(), expected)],
            "{command}"
        );
        assert_eq!(probe.confined.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn gh_read_missing_hosts_prompt_before_any_host_execution() {
    for (command, hosts, target) in [
        (
            "gh issue view 1 -R owner/repo",
            vec!["github.com"],
            "api.github.com",
        ),
        (
            "gh issue view 1 -R otherhost/o/r",
            vec!["github.com", "api.github.com"],
            "otherhost",
        ),
        (
            "gh issue view 1",
            vec!["github.com", "api.github.com"],
            "otherhost",
        ),
        (
            "gh issue view 1 -R tenant.ghe.com/o/r",
            vec!["tenant.ghe.com"],
            "api.tenant.ghe.com",
        ),
    ] {
        let probe = Probe {
            remote: "otherhost/o/r".into(),
            ..Default::default()
        };
        let mut caveats = granted();
        caveats.net = crate::Scope::only(hosts.into_iter().map(str::to_owned));
        let mut gate = Gate::default();
        let out = dispatch(command, &caveats, &probe, &mut gate).await;
        assert!(out.contains("refused"), "{out}");
        assert_eq!(gate.0.len(), 1, "{command}: {out}");
        assert_eq!(gate.0[0].target, target);
        assert_eq!(probe.host.load(Ordering::SeqCst), 0);
        assert_eq!(probe.confined.load(Ordering::SeqCst), 0);
    }
}

struct AllowGate {
    net: crate::Scope<String>,
    requests: Vec<String>,
}
impl PermissionGate for AllowGate {
    fn ask_dependency_fetch(
        &mut self,
        _prepared: &newt_policy::caveats::Caveats,
        _request: &newt_policy::permissions::PermissionRequest,
    ) -> newt_policy::permissions::PermissionDecision {
        newt_policy::permissions::PermissionDecision::Deny
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        panic!("unexpected question")
    }
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        self.requests
            .extend(requests.iter().map(|r| r.target.clone()));
        PermissionDecision::Allow(crate::Caveats {
            net: self.net.clone(),
            ..crate::Caveats::top()
        })
    }
}
#[tokio::test]
async fn gh_read_approval_is_authority_and_all_needs_no_prompt() {
    for allow in [true, false] {
        let probe = Probe::default();
        let mut gate = AllowGate {
            net: if allow {
                crate::Scope::All
            } else {
                crate::Scope::none()
            },
            requests: vec![],
        };
        let mut caveats = granted();
        caveats.net = crate::Scope::none();
        let out = dispatch("gh issue view 1 -R owner/repo", &caveats, &probe, &mut gate).await;
        assert_eq!(
            probe.host.load(Ordering::SeqCst),
            usize::from(allow),
            "{out}"
        );
        assert_eq!(
            gate.requests,
            if allow {
                vec!["github.com", "api.github.com"]
            } else {
                vec!["github.com"]
            }
        );
    }
    let probe = Probe::default();
    let mut gate = Gate::default();
    let mut caveats = granted();
    caveats.net = crate::Scope::All;
    let out = dispatch(
        "gh issue view 1 -R otherhost/o/r",
        &caveats,
        &probe,
        &mut gate,
    )
    .await;
    assert!(out.contains("issue 1"), "{out}");
    assert!(gate.0.is_empty());
    assert_eq!(probe.calls.lock().unwrap()[0].0, "otherhost");
}

#[tokio::test]
async fn gh_read_refuses_unsafe_flags_and_shell_topology_without_a_spawn() {
    for command in [
        "gh issue view 1 --web",
        "gh pr view 1 -w",
        "gh pr checks 1 --watch",
        "gh issue view 1 --template '{{exec}}'",
        "gh issue list --jq .",
        "gh pr diff 1 --color always",
        "gh issue view 1 --body-file x",
        "gh issue list --limit 101",
        "gh issue list --limit 0",
        "gh issue list --state bogus",
        "gh issue view https://otherhost/o/r/issues/1",
        "gh pr view branch",
        "gh pr view",
        "gh issue view 1 -R o/r -R otherhost/o/r",
        "gh issue view 1 -R https://otherhost/o/r",
        "gh issue view 1 -R otherhost:443/o/r",
        "gh issue view 1 -R api.github.com/o/r",
        "gh issue view 1 -R www.github.com/o/r",
        "gh issue view 1 -R api.tenant.ghe.com/o/r",
        "gh issue view 1 > output",
        "gh issue view 1 | cat",
        "gh issue view 1 && echo done",
        "gh issue view 1; touch output",
        "GH_HOST=otherhost gh issue view 1",
        "gh issue view $(echo 1)",
    ] {
        let probe = Probe::default();
        let mut gate = Gate::default();
        let out = dispatch(command, &granted(), &probe, &mut gate).await;
        assert!(out.contains("refused"), "{command}: {out}");
        assert_eq!(probe.host.load(Ordering::SeqCst), 0, "{command}");
        assert_eq!(probe.confined.load(Ordering::SeqCst), 0, "{command}");
        assert!(gate.0.is_empty(), "{command}");
    }
}

#[test]
fn gh_read_unlisted_verbs_are_not_brokered() {
    for command in [
        "gh api user",
        "gh alias list",
        "gh extension list",
        "gh auth status",
        "gh repo clone o/r",
        "gh issue edit 1",
        "gh pr create",
        "gh unknown",
        "echo 'gh issue view 1'",
    ] {
        assert!(!native_git::gh_read::recognizes(command), "{command}");
    }
}

#[tokio::test]
async fn gh_read_requires_exec_and_read_authority_and_bounds_output() {
    for exec in [true, false] {
        let mut caveats = granted();
        if exec {
            caveats.exec = crate::Scope::none();
        } else {
            caveats.fs_read = crate::Scope::none();
        }
        let probe = Probe::default();
        let mut gate = Gate::default();
        let out = dispatch("gh issue view 1 -R o/r", &caveats, &probe, &mut gate).await;
        assert!(out.contains("required"), "{out}");
        assert_eq!(probe.host.load(Ordering::SeqCst), 0);
    }
    let probe = Probe {
        output: "long output\n".repeat(10000),
        ..Default::default()
    };
    let mut gate = Gate::default();
    let out = dispatch("gh issue view 1 -R o/r", &granted(), &probe, &mut gate).await;
    assert!(out.len() < 25000, "output was not capped: {}", out.len());
    let out = dispatch(
        "gh issue view 1 -R o/r | head -1; echo done",
        &granted(),
        &probe,
        &mut gate,
    )
    .await;
    assert_eq!(out, "long output\ndone");
}

#[test]
fn gh_read_literal_origin_ignores_includes_and_rewrites_without_discovery() {
    let config = "[include]\npath = /never/read\n[remote \"origin\"]\nurl = https://github.com/o/r.git\n[url \"https://otherhost/\"]\ninsteadOf = https://github.com/\n";
    assert_eq!(
        crate::git_staging::literal_remote_from_config(config, "remote.origin.url").unwrap(),
        "https://github.com/o/r.git"
    );
    assert!(crate::git_staging::literal_remote_from_config(
        "[include]\npath = /never/read",
        "remote.origin.url"
    )
    .is_err());
}

#[tokio::test]
async fn gh_read_nested_commands_follow_pr_create_recognition_exactly() {
    for (read, create) in [
        ("(gh issue view 1)", "(gh pr create)"),
        ("echo $(gh issue view 1)", "echo $(gh pr create)"),
    ] {
        let recognized = native_git::gh_read::recognizes(read);
        assert_eq!(recognized, native_git::needs_pr_create_broker(create));
        let probe = Probe::default();
        let mut gate = Gate::default();
        let out = dispatch(read, &granted(), &probe, &mut gate).await;
        assert_eq!(probe.host.load(Ordering::SeqCst), 0, "{read}: {out}");
        assert_eq!(
            probe.confined.load(Ordering::SeqCst),
            usize::from(!recognized),
            "{read}: {out}"
        );
        assert!(gate.0.is_empty());
    }
}

#[test]
fn gh_read_literal_arguments_are_not_classified_as_commands() {
    for (source, expected) in [
        ("o/r", "o/r"),
        ("otherhost/o/r", "otherhost/o/r"),
        ("done", "done"),
        ("'two words'", "two words"),
        ("'$(touch file)'", "$(touch file)"),
        ("''", ""),
    ] {
        assert_eq!(
            agent_bridle_tool_shell::literal_shell_word(source).unwrap(),
            expected
        );
    }
    for source in [
        "a b",
        "x; id",
        "x # comment",
        "x > file",
        "$(id)",
        "$HOME",
        "*.rs",
        "{a,b}",
        "x\ny",
        "a && b",
        "~",
        "`id`",
    ] {
        assert!(
            agent_bridle_tool_shell::literal_shell_word(source).is_err(),
            "{source}"
        );
    }
}

#[tokio::test]
async fn gh_read_failed_and_pending_checks_keep_stdout_and_wrapper_status() {
    for code in [1, 8] {
        let probe = Probe {
            output: "build pending\nlint failed\n".into(),
            exit_code: code,
            ..Default::default()
        };
        for (wrapper, expected) in [
            ("", "build pending\nlint failed".to_string()),
            ("; echo status=$?", format!("status={code}")),
            (
                " | head -1; echo status=$?",
                "build pending\nstatus=0".into(),
            ),
        ] {
            let out = dispatch(
                &format!("gh pr checks 1 -R o/r{wrapper}"),
                &granted(),
                &probe,
                &mut Gate::default(),
            )
            .await;
            assert!(out.contains(&expected), "lost read output/status: {out}");
            assert!(out.contains(&format!("(exit {code})")), "{out}");
            assert!(!out.contains("credentials"), "{out}");
        }
    }
}

#[tokio::test]
async fn gh_read_decline_has_one_refused_prefix() {
    let out = dispatch(
        "gh issue view 1 -R otherhost/o/r",
        &granted(),
        &Probe::default(),
        &mut Gate::default(),
    )
    .await;
    assert_eq!(out.matches("refused:").count(), 1, "{out}");
}

#[tokio::test]
async fn gh_read_empty_failures_do_not_blame_credentials_and_large_failures_keep_status() {
    for output in [String::new(), "check pending\n".repeat(10000)] {
        let probe = Probe {
            output,
            exit_code: 8,
            ..Default::default()
        };
        let out = dispatch(
            "gh pr checks 1 -R o/r",
            &granted(),
            &probe,
            &mut Gate::default(),
        )
        .await;
        assert!(out.ends_with("(exit 8)"), "{out}");
        assert!(!out.contains("credentials"));
        if probe.output.is_empty() {
            assert!(out.contains("refused: gh read returned no output"));
        } else {
            assert!(out.len() < 25000, "unbounded read result");
        }
    }
}
