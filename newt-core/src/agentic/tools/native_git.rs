//! Transitional native Git checks, not a replacement for a repository broker.
//! Keep the caller's shell source unchanged and preserve the embedded adapter's
//! destructive-operation confirmations while the command interface migrates.

use super::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use crate::caveats::{Caveats, CaveatsExt};
use crate::git_caveats::GitCaveats;
use agent_bridle::{inspect_shell, ShellInspection};
use std::path::{Path, PathBuf};

pub(super) fn preflight(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    native_commit_broker: bool,
) -> Result<(), String> {
    let inspection = match inspect_shell(source) {
        Ok(inspection) => inspection,
        // Interpreter/dispatcher execution remains the confined executor's
        // responsibility. Text mentioning Git is not proof of execution.
        // Descendant/dynamic ref semantics still require a repository broker;
        // this direct-command safeguard must not pretend to supply one.
        Err(_) => return Ok(()),
    };
    let caps = GitCaveats::from_session(caveats);
    inspect_commands(
        &inspection,
        true,
        cwd,
        caveats,
        &caps,
        gate,
        native_commit_broker,
    )
}

pub(super) fn needs_commit_broker(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_git)
                && invocation(&command.argv)
                    .ok()
                    .and_then(|(verb, _, _)| literal(verb))
                    .as_deref()
                    == Some("commit")
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

fn unresolved(detail: &str) -> String {
    format!("refused: {detail}; native Git ref mutation is not yet supported by the repository authority adapter")
}

fn is_gh(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("gh") || name.eq_ignore_ascii_case("gh.exe")
}

/// issue-1188: is `source` a `git push` invocation anywhere in it (loose
/// trigger, same shape as [`needs_commit_broker`])? A `true` here routes the
/// WHOLE `run_command` call to [`execute_governed_push`] instead of the
/// confined shell — that function applies the strict standalone/fixed-argv
/// check and refuses anything this loose scan admits but the broker cannot
/// safely execute (composed commands, extra flags, a non-matching branch).
pub(super) fn needs_push_broker(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_git)
                && invocation(&command.argv)
                    .ok()
                    .and_then(|(verb, _, _)| literal(verb))
                    .as_deref()
                    == Some("push")
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

/// issue-1188 amendment A4: is `source` a `gh pr create` invocation anywhere
/// in it? Same loose-trigger/strict-execute split as [`needs_push_broker`].
pub(super) fn needs_pr_create_broker(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_gh)
                && command.argv.get(1).and_then(|a| literal(a)).as_deref() == Some("pr")
                && command.argv.get(2).and_then(|a| literal(a)).as_deref() == Some("create")
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

/// Is `source` a single, literal, standalone command (no compound/redirect/
/// descendant-exec/dynamic-argv shape)? Shared strictness gate for both
/// governed brokers below — anything else is refused rather than guessed at,
/// same posture as `inspect_commands`'s "composed or redirected Git mutation"
/// refusal.
fn standalone_literal_argv(source: &str) -> Result<Vec<String>, String> {
    let inspection =
        inspect_shell(source).map_err(|e| unresolved(&format!("shell inspection failed: {e}")))?;
    if inspection.commands.len() != 1 || !inspection.constructs.is_empty() {
        return Err(unresolved("composed or redirected Git/gh invocation"));
    }
    let command = &inspection.commands[0];
    if !command.redirects.is_empty() || !command.descendant_execs.is_empty() {
        return Err(unresolved("composed or redirected Git/gh invocation"));
    }
    command
        .argv
        .iter()
        .map(|arg| literal(arg).ok_or_else(|| unresolved("dynamic Git/gh arguments")))
        .collect()
}

/// issue-1188 amendment A1/A2: the exact push this broker will execute —
/// never taken verbatim from the model's argv, only VALIDATED against it. The
/// remote name is the only thing the model's command may choose; the branch
/// is always the workspace's OWN checked-out branch, resolved independently
/// (never trusted from argv), so an explicit branch/refspec operand must
/// match it exactly or the whole call is refused.
struct GovernedPush {
    remote: String,
    /// The `<branch>` half of an explicit `<remote> <branch>:<branch>`
    /// operand, if the model's command supplied one — never used to BUILD
    /// the eventual refspec (that is always the workspace's own resolved
    /// branch, fully qualified), only to verify the model's operand actually
    /// names that same branch (issue-1188 review #2641, finding 2: a
    /// `git push origin other:other` must not silently push the current
    /// branch under a name the model never asked for).
    requested_branch: Option<String>,
}

fn parse_governed_push(argv: &[String]) -> Result<GovernedPush, String> {
    let (verb, args, same_repository) = invocation(argv)?;
    let args = args.to_vec();
    if verb != "push" {
        return Err(unresolved("not a `git push` invocation"));
    }
    if !same_repository {
        return Err(unresolved(
            "`-C`/`--git-dir`/`--work-tree` on a governed push",
        ));
    }
    if args.iter().any(|a| a.starts_with('-')) {
        return Err(unresolved(
            "a governed push accepts no flags (no --force, --all, --mirror, --tags, -u, …)",
        ));
    }
    match args.len() {
        0 => Ok(GovernedPush {
            remote: "origin".to_string(),
            requested_branch: None,
        }),
        1 => Ok(GovernedPush {
            remote: args[0].clone(),
            requested_branch: None,
        }),
        2 => {
            let remote = args[0].clone();
            let refspec = &args[1];
            let (src, dst) = refspec.split_once(':').unwrap_or((refspec, refspec));
            if src != dst {
                return Err(unresolved(
                    "a governed push only pushes a branch to its own name",
                ));
            }
            Ok(GovernedPush {
                remote,
                requested_branch: Some(src.trim_start_matches("refs/heads/").to_string()),
            })
        }
        _ => Err(unresolved("unsupported governed push form")),
    }
}

/// issue-1188 #2641 design round 4: execute a governed `git push` through the
/// staging-repo broker (`crate::git_staging`), entirely host-side (never
/// through the confined shell — see the call site's comment in `tools.rs`).
/// `cwd` is the resolved directory the model's `run_command` targeted (after
/// any folded leading `cd`).
pub(super) fn execute_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let plan = plan_governed_push(source, cwd, caveats, gate)?;
    Ok(run_staged_push(&plan, caveats)?.to_string())
}

/// Everything [`execute_governed_push`] must verify BEFORE it may create a
/// staging repo at all: argv shape, executables (authenticated before ANY
/// subprocess), administrative-dir read authority, main/master refusal, the
/// literal destination URL, the PINNED source commit (F3: read from the ref
/// file and verified to be a commit by a confined `cat-file` — nothing
/// resolves a ref after this), and the net-gate prompt, which names that
/// commit. Split out so the tests can pin every refusal and the exact plan.
fn plan_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<GovernedPushPlan, String> {
    use crate::git_staging as staging;
    staging::preflight_availability(caveats).map_err(|e| e.to_string())?;

    let argv = standalone_literal_argv(source)?;
    let request = parse_governed_push(&argv)?;

    // Finding 6 (issue-1188 review #2641): the broker runs outside the
    // confined shell's fence, so it binds itself to the SAME authority a
    // confined command would have needed.
    // Prefix (containment) semantics, as at every fs enforcement site:
    // `permits_fs_read` is exact-match and would refuse a subdirectory.
    let cwd_str = cwd.to_string_lossy();
    if !crate::caveats::permits_path(&caveats.fs_read, &cwd_str) {
        return Err(format!(
            "refused: '{cwd_str}' is outside this session's filesystem read authority"
        ));
    }
    if !caveats.permits_exec("git") {
        return Err("refused: no exec authority for 'git'".to_string());
    }

    // F1/F2: authenticate git/gh/sh/exec-path BEFORE any planning subprocess.
    let tools = staging::TrustedTools::authenticate(&caveats.fs_write)?;
    // Administrative dirs by file reads, each read-authorized before use.
    let (common_dir, git_dir) = staging::discover_git_dirs(cwd, &caveats.fs_read)?;

    let branch = staging::read_head_branch(&git_dir)?;
    if staging::is_default_branch(&common_dir, &branch) {
        return Err(format!(
            "refused: cannot push the default branch '{branch}' (open a feature branch instead)"
        ));
    }
    // Finding 2: an explicit `<remote> <branch>:<branch>` operand must name
    // the SAME branch this broker is about to push.
    if let Some(requested) = &request.requested_branch {
        if requested != &branch {
            return Err(format!(
                "refused: requested branch '{requested}' does not match the checked-out \
                 branch '{branch}' — a governed push always pushes the workspace's own branch"
            ));
        }
    }

    // The LITERAL `remote.<name>.url` (never `git remote get-url`, which
    // applies insteadOf): what the operator approves is what staging dials.
    let url = staging::literal_remote_url(&tools, &common_dir, &request.remote)?;
    let host = crate::git_hardening::push_url_host(&url)?;
    let Some((owner, name)) = crate::git_hardening::github_owner_repo(&url) else {
        return Err(format!(
            "refused: a governed push is only supported for a github.com remote (got {url})"
        ));
    };

    // F3: pin the commit at PLAN time; it is carried to the dial unchanged.
    let oid = staging::read_branch_oid(&git_dir, &branch)
        .or_else(|_| staging::read_branch_oid(&common_dir, &branch))?;
    staging::resolve_alternates_chain(&common_dir.join("objects"), &caveats.fs_read)?;
    staging::verify_commit(&tools, &common_dir, &oid, caveats)?;

    ensure_net_granted(
        caveats,
        gate,
        &host,
        &format!("push commit {oid} as branch '{branch}' to {host} ({url})"),
    )?;

    Ok(GovernedPushPlan {
        url,
        owner,
        name,
        branch,
        oid,
        source_repo: common_dir,
        tools,
    })
}

/// The exact push [`execute_governed_push`] will stage and dial — the
/// approved commit and destination, resolved and net-gated but not yet
/// spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GovernedPushPlan {
    url: String,
    owner: String,
    name: String,
    branch: String,
    /// The commit the operator approved; the refspec's source.
    oid: String,
    /// The workspace REPOSITORY (common dir) the confined copy reads from.
    source_repo: PathBuf,
    tools: crate::git_staging::TrustedTools,
}

/// Phases 2-6 of the staging-repo broker (`crate::git_staging`) for a push
/// already planned and net-gated by [`plan_governed_push`]. Every child is
/// the plan's checked git with the allowlist env and captured output (F2/F6).
fn run_staged_push(
    plan: &GovernedPushPlan,
    caveats: &Caveats,
) -> Result<crate::git_staging::Outcome, String> {
    use crate::git_staging::{self, Outcome};

    let state_dir = crate::config::Config::user_config_dir()
        .ok_or_else(|| "refused: no resolvable state directory for a staging repo".to_string())?
        .join("staging");
    let staging = git_staging::StagingRepo::create(&state_dir, &caveats.fs_write)?;
    git_staging::import_credentials(&staging, &plan.tools, &caveats.fs_write)?;

    // F4: the ONLY step that reads workspace objects, kernel-confined.
    git_staging::confined_fetch(&plan.tools, &staging, &plan.source_repo, &plan.oid, caveats)?;
    git_staging::remove_alternates(&staging)?;
    let fsck = plan
        .tools
        .git(["fsck", "--connectivity-only", plan.oid.as_str()])
        .current_dir(staging.path())
        .output()
        .map_err(|e| e.to_string())?;
    if !fsck.status.success() {
        return Err(
            "refused: staging repo failed connectivity check after the confined fetch".to_string(),
        );
    }

    // F3: push the APPROVED oid; no ref is resolved after approval.
    let refspec = format!("{}:refs/heads/{}", plan.oid, plan.branch);
    let output = plan
        .tools
        .git([
            "-c",
            "http.followRedirects=false",
            "push",
            plan.url.as_str(),
            refspec.as_str(),
        ])
        .current_dir(staging.path())
        .output()
        .map_err(|e| e.to_string())?;
    Ok(if output.status.success() {
        Outcome::Pushed {
            oid: plan.oid.clone(),
            owner: plan.owner.clone(),
            name: plan.name.clone(),
            branch: plan.branch.clone(),
        }
    } else {
        Outcome::Failed {
            category: classify_failure(&output.stderr),
        }
    })
}

/// Best-effort classification of a failed dial from the child's stderr BYTES
/// — read only in-process to pick one of a small fixed set of words (F6); the
/// bytes themselves are never returned to the model, the terminal, or a log.
fn classify_failure(stderr: &[u8]) -> crate::git_staging::FailureCategory {
    use crate::git_staging::FailureCategory;
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if text.contains("could not resolve host")
        || text.contains("connection refused")
        || text.contains("connection timed out")
        || text.contains("network is unreachable")
    {
        FailureCategory::Network
    } else if text.contains("authentication failed")
        || text.contains("permission denied")
        || text.contains("403")
        || text.contains("401")
    {
        FailureCategory::Auth
    } else {
        FailureCategory::GitError
    }
}

/// issue-1188 amendment A4: the exact `gh pr create` this broker will
/// execute. Only `--title`/`-t` and `--body`/`-b` are accepted from the
/// model's argv; `--repo`, `--base`, and `--head` are always resolved by the
/// broker itself so `gh` never infers (or is told to use) anything else.
struct GovernedPrCreate {
    title: String,
    body: String,
}

fn parse_governed_pr_create(argv: &[String]) -> Result<GovernedPrCreate, String> {
    // argv[0] = "gh", argv[1] = "pr", argv[2] = "create", rest are flags.
    let mut title = None;
    let mut body = None;
    let mut i = 3;
    while i < argv.len() {
        let word = argv[i].as_str();
        let (flag, inline_value) = match word.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (word, None),
        };
        let mut take_value = || -> Result<String, String> {
            if let Some(v) = &inline_value {
                return Ok(v.clone());
            }
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| unresolved("gh pr create flag missing its value"))
        };
        match flag {
            "--title" | "-t" => title = Some(take_value()?),
            "--body" | "-b" => body = Some(take_value()?),
            _ => {
                return Err(unresolved(&format!(
                    "unsupported `gh pr create` flag '{flag}' — only --title/--body are \
                     accepted; --repo/--base/--head are resolved automatically"
                )))
            }
        }
        i += 1;
    }
    let title = title.ok_or_else(|| unresolved("gh pr create: --title is required"))?;
    let body = body.ok_or_else(|| unresolved("gh pr create: --body is required"))?;
    Ok(GovernedPrCreate { title, body })
}

/// issue-1188 #2641 design round 4: execute a governed `gh pr create` from a
/// fresh staging directory (Phase 4 — `cwd = staging`, so no workspace
/// `.git/config` is ever in scope for `gh`'s own git children).
pub(super) fn execute_governed_pr_create(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let plan = plan_governed_pr_create(source, cwd, caveats, gate)?;
    Ok(run_staged_pr_create(&plan, caveats)?.to_string())
}

/// Phase 2/4/5 for `gh pr create`: a bare staging dir (no objects needed —
/// `gh` never reads repository content to open a PR), the plan's checked gh
/// with the allowlist env, and the fixed-form outcome (F6): a `pr_created`
/// URL is validated by [`crate::git_staging::validate_pr_url`] or the outcome
/// degrades to `failed(parse)`.
fn run_staged_pr_create(
    plan: &GovernedPrCreatePlan,
    caveats: &Caveats,
) -> Result<crate::git_staging::Outcome, String> {
    use crate::git_staging::{self, FailureCategory, Outcome};

    let state_dir = crate::config::Config::user_config_dir()
        .ok_or_else(|| "refused: no resolvable state directory for a staging repo".to_string())?
        .join("staging");
    let staging = git_staging::StagingRepo::create(&state_dir, &caveats.fs_write)?;
    git_staging::import_credentials(&staging, &plan.tools, &caveats.fs_write)?;

    let repo = format!("github.com/{}", plan.repo);
    let output = plan
        .tools
        .gh([
            "pr",
            "create",
            "--repo",
            repo.as_str(),
            "--base",
            plan.base.as_str(),
            "--head",
            plan.head.as_str(),
            "--title",
            plan.title.as_str(),
            "--body",
            plan.body.as_str(),
        ])?
        .current_dir(staging.path())
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(Outcome::Failed {
            category: classify_failure(&output.stderr),
        });
    }
    Ok(
        match git_staging::validate_pr_url(&String::from_utf8_lossy(&output.stdout)) {
            Some(url) => Outcome::PrCreated { url },
            None => Outcome::Failed {
                category: FailureCategory::Parse,
            },
        },
    )
}

/// The exact `gh pr create` invocation [`execute_governed_pr_create`] will
/// run, resolved and net-gated but not yet spawned — same split rationale as
/// [`plan_governed_push`].
#[derive(Debug)]
struct GovernedPrCreatePlan {
    repo: String,
    base: String,
    head: String,
    title: String,
    body: String,
    tools: crate::git_staging::TrustedTools,
}

fn plan_governed_pr_create(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<GovernedPrCreatePlan, String> {
    use crate::git_staging as staging;
    staging::preflight_availability(caveats).map_err(|e| e.to_string())?;

    let argv = standalone_literal_argv(source)?;
    let request = parse_governed_pr_create(&argv)?;

    // Finding 6: same cwd authority/exec binding as the push broker.
    // Prefix (containment) semantics, as at every fs enforcement site:
    // `permits_fs_read` is exact-match and would refuse a subdirectory.
    let cwd_str = cwd.to_string_lossy();
    if !crate::caveats::permits_path(&caveats.fs_read, &cwd_str) {
        return Err(format!(
            "refused: '{cwd_str}' is outside this session's filesystem read authority"
        ));
    }
    if !caveats.permits_exec("gh") {
        return Err("refused: no exec authority for 'gh'".to_string());
    }

    // F1/F2: authenticate BEFORE any planning subprocess; gh is required.
    let tools = staging::TrustedTools::authenticate(&caveats.fs_write)?;
    if tools.gh_path().is_none() {
        return Err("refused: no trusted gh executable is installed".to_string());
    }
    let (common_dir, git_dir) = staging::discover_git_dirs(cwd, &caveats.fs_read)?;

    // The `gh` dial runs from a fresh staging directory (Phase 4), so a
    // repo-local config gadget has nothing to reach.
    let url = staging::literal_remote_url(&tools, &common_dir, "origin")?;
    let Some((owner, name)) = crate::git_hardening::github_owner_repo(&url) else {
        return Err(format!(
            "refused: gh pr create is only supported for a github.com 'origin' remote (got {url})"
        ));
    };
    let host = "github.com".to_string();

    let head = staging::read_head_branch(&git_dir)
        .map_err(|_| "refused: detached HEAD has no branch to open a PR from".to_string())?;
    let base = staging::origin_default_branch(&common_dir).unwrap_or_else(|| "main".to_string());
    if head == base {
        return Err(format!(
            "refused: cannot open a PR from the default branch '{base}' to itself"
        ));
    }

    ensure_net_granted(
        caveats,
        gate,
        &host,
        &format!("open a PR on {owner}/{name}"),
    )?;

    Ok(GovernedPrCreatePlan {
        repo: format!("{owner}/{name}"),
        base,
        head,
        title: request.title,
        body: request.body,
        tools,
    })
}

/// issue-1188 amendment A5: fold into the existing `net:<host>` vocabulary —
/// the same [`DenialKind::Net`]/[`PermissionRequest`] shape `web_fetch`
/// already uses (`tools.rs`'s `web_fetch` arm), so a granted host from either
/// path satisfies the other, and durable persistence (if the concrete gate
/// offers it) is the SAME store, not a second one.
fn ensure_net_granted(
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    host: &str,
    action: &str,
) -> Result<(), String> {
    if caveats.permits_net(host) {
        return Ok(());
    }
    let Some(gate) = gate.as_deref_mut() else {
        return Err(format!(
            "refused: no network authority for '{host}' and no operator to ask"
        ));
    };
    let request = PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::Net,
        target: host.to_string(),
        reason: format!("{action} — net does not permit '{host}'"),
    };
    match gate.ask(std::slice::from_ref(&request)) {
        // Finding 6 (issue-1188 review #2641): the returned capability is the
        // authority — an `Allow` that does not actually cover `host` (a gate
        // that granted a DIFFERENT host, or nothing at all) must not be read
        // as a yes just because the variant is `Allow`.
        PermissionDecision::Allow(granted) if granted.permits_net(host) => Ok(()),
        PermissionDecision::Allow(_) => Err(format!(
            "refused: granted authority does not cover network access to '{host}'"
        )),
        PermissionDecision::Deny => Err(format!(
            "refused: operator did not grant network access to '{host}'"
        )),
    }
}

fn is_git(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("git") || name.eq_ignore_ascii_case("git.exe")
}

/// Git for Windows resolves its startup current directory through every
/// profile ancestor. AppContainer deliberately cannot read those ancestors
/// merely because the repository itself is admitted, so executing native Git
/// would otherwise produce an opaque child error after the authority decision.
///
/// Keep this a named, pre-spawn refusal instead of widening the filesystem
/// fence or falling back to the host. An operator may still make the explicit
/// `--disable-ocap` / `--full-access` choice, which selects a non-AppContainer
/// route before this check runs.
#[cfg(any(target_os = "windows", test))]
pub(super) const WINDOWS_APPCONTAINER_GIT_UNAVAILABLE: &str =
    "native Git is unavailable under Windows AppContainer confinement: Git for Windows requires current-directory ancestry traversal outside the granted roots; no command ran";

/// A single, literal direct native-Git command. Compound and dynamic forms
/// retain their normal shell semantics: refusing a whole compound before its
/// earlier stages run would be a separate behavior change.
#[cfg(any(target_os = "windows", test))]
fn literal_native_git_program(source: &str) -> Option<String> {
    let inspection = inspect_shell(source).ok()?;
    let command = inspection.commands.first()?;
    if inspection.commands.len() == 1
        && inspection.constructs.is_empty()
        && command.redirects.is_empty()
        && command.descendant_execs.is_empty()
    {
        command
            .program
            .as_deref()
            .filter(|program| is_git(program))
            .map(str::to_owned)
    } else {
        None
    }
}

#[cfg(any(target_os = "windows", test))]
fn appcontainer_native_git_refusal_for(
    program: Option<&str>,
    effective_sandbox: agent_bridle::SandboxKind,
    exec_allowed: bool,
) -> Option<&'static str> {
    (program.is_some_and(is_git)
        && exec_allowed
        && effective_sandbox == agent_bridle::SandboxKind::AppContainer)
        .then_some(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE)
}

/// Mirror Bridle's executable authority check for a direct program word. A
/// bare `git.exe` grant also authorizes a PATH-resolved `...\\git.exe`, so the
/// pre-spawn refusal must recognize that exact effective authority rather than
/// letting the eventual interceptor reach AppContainer first.
#[cfg(target_os = "windows")]
fn exec_scope_allows_program(caveats: &Caveats, program: &str) -> bool {
    caveats.permits_exec(program)
        || Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| caveats.permits_exec(name))
}

/// A Windows-only pre-spawn refusal for the one native Git shape known to be
/// incompatible with the restricted AppContainer backend. Its backend choice
/// is calculated from the same policy as the eventual shell dispatch; an
/// unrestricted grant therefore stays on its existing non-AppContainer path.
pub(super) fn windows_appcontainer_native_git_refusal(
    source: &str,
    caveats: &Caveats,
) -> Option<&'static str> {
    #[cfg(target_os = "windows")]
    {
        let program = literal_native_git_program(source);
        let exec_allowed = program
            .as_deref()
            .is_some_and(|program| exec_scope_allows_program(caveats, program));
        let policy = std::sync::Arc::new(crate::confined_exec::runtime_sandbox_policy());
        let effective_sandbox = agent_bridle::effective_sandbox_kind(
            agent_bridle::best_available_sandbox(&policy).kind(),
            caveats,
        );
        appcontainer_native_git_refusal_for(program.as_deref(), effective_sandbox, exec_allowed)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (source, caveats);
        None
    }
}

/// Reuse Bridle's static executable-word resolution for a single literal
/// argument. No shell is run; expansions, multiple words, and redirects fail.
fn literal(word: &str) -> Option<String> {
    let parsed = inspect_shell(word).ok()?;
    let command = parsed.commands.first()?;
    (parsed.commands.len() == 1
        && parsed.constructs.is_empty()
        && command.argv.len() == 1
        && command.redirects.is_empty()
        && command.descendant_execs.is_empty())
    .then(|| command.program.clone())
    .flatten()
}

/// Find the native verb without translating argv. Repository/config selectors
/// remain usable for reads, but mutation cannot rely on the caller's cwd then.
fn invocation(argv: &[String]) -> Result<(&str, &[String], bool), String> {
    let mut index = 1;
    let mut same_repository = true;
    while let Some(word) = argv.get(index) {
        match word.as_str() {
            "--no-pager" | "--paginate" | "--no-optional-locks" => index += 1,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                same_repository = false;
                index += 2;
            }
            option
                if option.starts_with("--git-dir=")
                    || option.starts_with("--work-tree=")
                    || option.starts_with("--namespace=")
                    || option.starts_with("--config-env=")
                    || option.starts_with("--exec-path=")
                    || option == "--bare" =>
            {
                same_repository = false;
                index += 1;
            }
            "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-replace-objects" => index += 1,
            "--version" | "--help" | "--html-path" | "--man-path" | "--info-path" => {
                return Ok(("help", &[], true));
            }
            verb if !verb.starts_with('-') => {
                return Ok((verb, &argv[index + 1..], same_repository));
            }
            _ => return Err(unresolved("unresolved Git global options")),
        }
    }
    Ok(("help", &[], true))
}

fn inspect_commands(
    inspection: &ShellInspection,
    top_level: bool,
    cwd: &Path,
    caveats: &Caveats,
    caps: &GitCaveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    native_commit_broker: bool,
) -> Result<(), String> {
    for command in &inspection.commands {
        for descendant in &command.descendant_execs {
            if is_git(&descendant.program) {
                let mut delegated = command.clone();
                delegated.source = descendant.source.clone();
                delegated.program = Some(descendant.program.clone());
                delegated.argv = descendant.argv.clone();
                delegated.redirects.clear();
                delegated.descendant_execs.clear();
                let nested = ShellInspection {
                    schema_version: inspection.schema_version,
                    source: descendant.source.clone(),
                    commands: vec![delegated],
                    constructs: vec![],
                    warnings: vec![],
                };
                inspect_commands(
                    &nested, false, cwd, caveats, caps, gate,
                    // Native dispatchers spawn outside Brush's final command
                    // filter; another direct command's broker cannot cover them.
                    false,
                )?;
            }
        }
        if !command.program.as_deref().is_some_and(is_git) {
            continue;
        }
        let Ok((raw_verb, args, same_repository)) = invocation(&command.argv) else {
            // Unknown CLI forms retain Git's own interpretation and the
            // existing confined executor's authority checks.
            continue;
        };
        let Some(verb) = literal(raw_verb) else {
            continue;
        };
        match verb.as_str() {
            "commit" if native_commit_broker => continue,
            // Complement the preexisting lexical attribution guard for
            // quoted executable/verb spellings resolved by Bridle.
            "commit" | "merge" | "rebase" | "cherry-pick" | "revert" => {
                let aborting = verb != "commit"
                    && args
                        .iter()
                        .filter_map(|arg| literal(arg))
                        .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"));
                if !aborting {
                    return Err("refused: native commit creation requires harness-managed attribution and signing integration".into());
                }
                continue;
            }
            "checkout" | "switch" => {
                // -b/-c consumes its attached branch name; characters in
                // `-bfeature` / `-cfeature` are not bundled force options.
                let create_option = if verb == "checkout" { "-b" } else { "-c" };
                let words: Vec<String> = args
                    .iter()
                    .filter_map(|arg| literal(arg))
                    .filter(|word| !word.starts_with(create_option))
                    .collect();
                if has_option(
                    &words,
                    "BCf",
                    &["--force", "--force-create", "--discard-changes"],
                ) {
                    return Err(unresolved("forced branch replacement or checkout"));
                }
                continue;
            }
            "update-ref" => return Err(unresolved("direct ref update")),
            // Finding 7 (issue-1188 review #2641): a standalone top-level
            // `git push` never reaches this preflight at all —
            // `needs_push_broker` diverts it to the governed broker before
            // `preflight` is even called. Anything that DOES land here
            // (composed, redirected, or reached only through a descendant
            // dispatcher like `timeout`/`find -exec`) must be refused
            // outright rather than falling through to the confined shell,
            // where it would run ungoverned by the broker's branch/force
            // fixed-argv guarantees if net authority happened to be broader
            // than the `net: none` narrowing this whole broker exists to
            // route around.
            "push" => return Err(unresolved(
                "`git push` must be a single, standalone command handled by the governed push broker",
            )),
            "symbolic-ref" => {
                let words = literal_arguments(args)?;
                let query_flags = ["-q", "--quiet", "--short", "--recurse", "--no-recurse"];
                let operands: Vec<_> = words
                    .iter()
                    .filter(|word| !query_flags.contains(&word.as_str()))
                    .collect();
                if operands.len() != 1 || operands[0].starts_with('-') {
                    return Err(unresolved("symbolic ref mutation"));
                }
                continue;
            }
            "reflog" => {
                if let Some(first) = args.first() {
                    let action =
                        literal(first).ok_or_else(|| unresolved("dynamic reflog operation"))?;
                    if matches!(action.as_str(), "expire" | "delete" | "drop" | "write") {
                        return Err(unresolved("reflog mutation"));
                    }
                }
                continue;
            }
            "stash" => {
                let action = args
                    .first()
                    .map(|word| literal(word).ok_or_else(|| unresolved("dynamic stash operation")))
                    .transpose()?;
                if !matches!(action.as_deref(), Some("drop" | "clear")) {
                    continue;
                }
            }
            "branch" => {}
            // The existing attribution guard runs before this preflight.
            // This is a small destructive-operation guard, not a replacement
            // command catalog: all other Git verbs execute normally.
            _ => continue,
        }
        let words = literal_arguments(args)?;
        if verb == "branch" {
            if has_option(&words, "fmMcC", &["--force", "--move", "--copy"]) {
                return Err(unresolved("branch overwrite, rename, or copy"));
            }
            if !has_option(&words, "dD", &["--delete"]) {
                continue;
            }
        }
        // The flattened inventory deliberately does not promise shell state
        // or control-flow edges. Only a single literal invocation may use the
        // cwd-bound destructive check; never guess after cd/env/substitution.
        let standalone = top_level
            && inspection.commands.len() == 1
            && inspection.constructs.is_empty()
            && command.redirects.is_empty()
            && command.descendant_execs.is_empty()
            && command.source.trim_start().starts_with(&command.argv[0]);
        if !standalone || !same_repository {
            return Err(unresolved("composed or redirected Git mutation"));
        }
        let op = if verb == "branch" {
            let branches = deleted_branches(&words)?;
            for branch in branches {
                if !caps.permits_ref(&format!("refs/heads/{branch}")) {
                    return Err("refused: native branch deletion requires git-ref authority".into());
                }
                refuse_protected_branch(cwd, branch, caveats)?;
            }
            "branch-delete"
        } else {
            if !caps.permits_stage() {
                return Err("refused: native stash deletion requires git-write authority".into());
            }
            "stash-drop"
        };
        if !gate
            .as_deref_mut()
            .is_some_and(|gate| super::git_data_loss_confirmed(gate, op))
        {
            return Err(format!(
                "refused: git {op} requires explicit destructive-operation confirmation"
            ));
        }
    }
    for construct in &inspection.constructs {
        if let Some(nested) = &construct.inspection {
            inspect_commands(
                nested,
                false,
                cwd,
                caveats,
                caps,
                gate,
                native_commit_broker,
            )?;
        }
    }
    Ok(())
}

fn literal_arguments(args: &[String]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|arg| literal(arg).ok_or_else(|| unresolved("dynamic Git ref or stash arguments")))
        .collect()
}

fn has_option(words: &[String], short: &str, long: &[&str]) -> bool {
    words
        .iter()
        .take_while(|word| word.as_str() != "--")
        .any(|word| {
            long.contains(&word.as_str())
                || (word.starts_with('-')
                    && !word.starts_with("--")
                    && word[1..].chars().any(|flag| short.contains(flag)))
        })
}

fn deleted_branches(words: &[String]) -> Result<Vec<&str>, String> {
    let mut deleting = false;
    let mut positional = false;
    let mut branches = vec![];
    for word in words {
        match word.as_str() {
            "--" if !positional => positional = true,
            "-d" | "-D" | "--delete" if !positional => deleting = true,
            "-q" | "--quiet" if !positional => {}
            option if !positional && option.starts_with('-') => {
                return Err(unresolved("unsupported branch mutation options"))
            }
            branch => branches.push(branch),
        }
    }
    if !deleting || branches.is_empty() {
        return Err(unresolved("unclassified branch mutation"));
    }
    Ok(branches)
}

fn refuse_protected_branch(cwd: &Path, branch: &str, caveats: &Caveats) -> Result<(), String> {
    if matches!(branch, "main" | "master") {
        return Err(format!(
            "refused: cannot delete protected default branch '{branch}'"
        ));
    }
    // metadata_git checks read authority before constructing or launching any
    // process. Do not turn a bounded read grant into ambient repository reads.
    let output = crate::git_hardening::metadata_git(
        cwd,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
        &caveats.fs_read,
    )
    .and_then(|mut command| command.output())
    .map_err(|_| {
        unresolved("cannot establish the protected default branch under current read authority")
    })?;
    if output.status.success() {
        if String::from_utf8_lossy(&output.stdout)
            .trim()
            .strip_prefix("refs/remotes/origin/")
            == Some(branch)
        {
            return Err(format!(
                "refused: cannot delete protected default branch '{branch}'"
            ));
        }
    } else if output.status.code() != Some(1) {
        return Err(unresolved("cannot establish the repository default branch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        disable_ocap_tests::{env_lock, EnvVar},
        execute_tool_with_collaborators, PermissionDecision, PermissionRequest, ToolCollaborators,
    };
    use super::*;
    use crate::agentic::{NoMcp, PromptDisposition};

    #[test]
    fn python_text_mentioning_git_keeps_the_existing_execution_path() {
        for source in [
            "python -c 'print(\"git branch -D example\")'",
            "git status && python -m pytest",
        ] {
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, false).is_ok(),
                "{source}"
            );
        }
    }

    #[test]
    fn native_broker_selection_preserves_compound_and_quoted_commit_forms() {
        for source in [
            "git add .gitignore && git commit -m 'Ignore bytecode'",
            "'git' 'commit' -m message",
            "git -C directory -c user.email=author@example.invalid commit -F message.txt",
            "printf 'message' | git commit -F -",
        ] {
            assert!(needs_commit_broker(source), "{source}");
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, true).is_ok(),
                "{source}"
            );
        }
        assert!(!needs_commit_broker("python -c 'print(\"git commit\")'"));
        assert!(preflight(
            "git rebase HEAD~1",
            Path::new("."),
            &Caveats::top(),
            &mut None,
            true
        )
        .is_err());
    }

    /// Windows AppContainer cannot run Git for Windows from a profile-backed
    /// workspace: Git's startup cwd resolution needs ancestor access outside
    /// the admitted filesystem roots.  The route must name that limitation
    /// before spawning, while ordinary text that merely mentions Git remains
    /// eligible for the normal shell path.
    #[test]
    fn appcontainer_refuses_recognized_native_git_without_matching_text() {
        assert_eq!(
            literal_native_git_program("git status").as_deref(),
            Some("git")
        );
        assert_eq!(literal_native_git_program("echo git status"), None);
        assert_eq!(literal_native_git_program("git status && echo done"), None);
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::AppContainer,
                true,
            ),
            Some(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::AppContainer,
                false,
            ),
            None,
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::Landlock,
                true,
            ),
            None,
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git.exe"),
                agent_bridle::SandboxKind::None,
                true,
            ),
            None,
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn appcontainer_git_refusal_mirrors_bare_name_exec_grants() {
        let caveats = Caveats {
            exec: crate::Scope::only(["git.exe".to_owned()]),
            ..Caveats::top()
        };
        assert!(exec_scope_allows_program(
            &caveats,
            r"C:\Program Files\Git\cmd\git.exe"
        ));
        assert!(!exec_scope_allows_program(
            &caveats,
            r"C:\Program Files\Git\cmd\not-git.exe"
        ));
    }

    struct Gate {
        allow: bool,
        requests: Vec<PermissionRequest>,
    }

    #[test]
    fn native_descendants_cannot_borrow_a_direct_commits_broker() {
        for source in [
            "git commit -m direct; find . -exec git commit -m delegated \\;",
            "git commit -m direct; timeout 30 git commit -m delegated",
        ] {
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, true).is_err(),
                "a native descendant has no Brush spawn registration: {source}"
            );
        }
        assert!(
            preflight(
                "echo \"$(git commit -m direct)\"",
                Path::new("."),
                &Caveats::top(),
                &mut None,
                true,
            )
            .is_ok(),
            "actual nested Brush commands keep broker coverage"
        );
    }

    impl PermissionGate for Gate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.requests.extend_from_slice(requests);
            if self.allow {
                PermissionDecision::Allow(Caveats::top())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_question(&mut self, _question: &str) -> crate::agentic::HumanQuestionOutcome {
            crate::agentic::HumanQuestionOutcome::Unavailable
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = crate::git_hardening::hardened_git(cwd, args)
            .unwrap()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "original\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Native fixture",
                "-c",
                "user.email=native@example.invalid",
                "commit",
                "--no-gpg-sign",
                "-qm",
                "fixture",
            ],
        );
        for branch in ["master", "stable", "victim", "task"] {
            git(repo.path(), &["branch", branch]);
        }
        git(
            repo.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/stable",
            ],
        );
        git(repo.path(), &["checkout", "-q", "task"]);
        std::fs::write(repo.path().join("tracked.txt"), "saved work\n").unwrap();
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Native fixture",
                "-c",
                "user.email=native@example.invalid",
                "stash",
                "push",
                "-qm",
                "saved work",
            ],
        );
        repo
    }

    async fn run(source: &str, repo: &Path, gate: Option<&mut dyn PermissionGate>) -> String {
        execute_tool_with_collaborators(
            "run_command",
            &serde_json::json!({"command": source}),
            &repo.to_string_lossy(),
            false,
            40,
            &Caveats::top(),
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: gate,
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap()
    }

    /// Grounds permission refresh in Git's real index: a workspace grant
    /// approved during the turn must reach the confined child, while a later
    /// mutation without that grant must leave the index unchanged.
    #[tokio::test]
    async fn native_staging_uses_current_workspace_grants() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _full = EnvVar::unset("NEWT_FULL_ACCESS");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let workspace = repo.path().canonicalize().unwrap();
        let workspace = workspace.to_string_lossy().into_owned();
        std::fs::write(repo.path().join("pending.txt"), "pending work\n").unwrap();
        let baseline = Caveats {
            fs_write: crate::Scope::none(),
            ..Caveats::top()
        };

        struct RecalledWorkspaceGrant {
            workspace: Option<String>,
            refreshes: usize,
            requests: Vec<PermissionRequest>,
        }
        impl PermissionGate for RecalledWorkspaceGrant {
            fn refresh_caveats(&mut self, baseline: &Caveats) -> PermissionDecision {
                self.refreshes += 1;
                let mut current = baseline.clone();
                if let Some(workspace) = &self.workspace {
                    current.fs_write = crate::Scope::only([workspace.clone()]);
                }
                PermissionDecision::Allow(current)
            }
            fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
                self.requests.extend_from_slice(requests);
                PermissionDecision::Deny
            }
            fn ask_question(&mut self, _: &str) -> crate::agentic::HumanQuestionOutcome {
                crate::agentic::HumanQuestionOutcome::Unavailable
            }
        }

        for allowed in [true, false] {
            let mut gate = RecalledWorkspaceGrant {
                workspace: allowed.then(|| workspace.clone()),
                refreshes: 0,
                requests: vec![],
            };
            let command = if allowed {
                "git add -- pending.txt"
            } else {
                "git rm --cached -- pending.txt"
            };
            let out = execute_tool_with_collaborators(
                "run_command",
                &serde_json::json!({
                    "command": command,
                    "fs_write": [workspace],
                }),
                &workspace,
                false,
                40,
                &baseline,
                &mut NoMcp,
                ToolCollaborators {
                    permission_gate: Some(&mut gate),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let staged = git(repo.path(), &["diff", "--cached", "--name-only"]);
            #[cfg(target_os = "windows")]
            {
                if allowed {
                    assert!(
                        out.contains(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
                        "the recalled filesystem grant must reach the pre-spawn Windows guard: {out}"
                    );
                } else {
                    assert!(out.contains("capability denied"), "{out}");
                    assert!(
                        !out.contains(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
                        "a denied filesystem declaration must win before the Windows guard: {out}"
                    );
                }
                assert_eq!(staged.trim(), "", "{out}");
            }
            #[cfg(not(target_os = "windows"))]
            assert_eq!(staged.trim(), "pending.txt", "{out}");
            assert_eq!(gate.refreshes, 1, "allowed={allowed}: {out}");
            if allowed {
                assert!(gate.requests.is_empty(), "recalled grant must be reused");
            } else {
                assert!(out.contains("capability denied"), "{out}");
                assert_eq!(gate.requests.len(), 1);
                assert_eq!(gate.requests[0].kind, super::super::DenialKind::FsWrite);
                assert_eq!(gate.requests[0].target, workspace);
            }
        }
    }

    #[tokio::test]
    async fn denied_native_deletion_preserves_real_branches_and_stash() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let before = git(repo.path(), &["show-ref"]);
        let stash = git(repo.path(), &["stash", "list"]);
        for source in [
            "git branch -D victim",
            "git branch -d victim",
            "git stash drop 'stash@{0}'",
            "git stash clear",
        ] {
            let mut gate = Gate {
                allow: false,
                requests: vec![],
            };
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.starts_with("refused:"), "{source}: {out}");
            assert_eq!(gate.requests.len(), 1, "{source}: {out}");
            assert!(gate.requests[0].reason.contains("DESTROYS"));
            assert_eq!(git(repo.path(), &["show-ref"]), before, "{source}");
            assert_eq!(git(repo.path(), &["stash", "list"]), stash, "{source}");
        }
        let out = run("git branch -D victim", repo.path(), None).await;
        assert!(out.starts_with("refused:"), "{out}");
        assert_eq!(git(repo.path(), &["show-ref"]), before);
    }

    #[tokio::test]
    async fn protected_or_unresolved_native_ref_mutations_preserve_real_refs() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let repo = repository();
        let before = git(repo.path(), &["show-ref"]);
        for source in [
            "git branch -D main",
            "git branch -D master",
            "git branch -D stable",
            "git update-ref -d refs/heads/main",
            "git symbolic-ref HEAD refs/heads/main",
            "git branch -f main HEAD",
            "git branch -M main",
            "git switch -C main HEAD",
            "git checkout -B main HEAD",
            "git checkout --force main",
            "git reflog delete 'HEAD@{0}'",
            "git -C . branch -D victim",
            "git branch -D \"$TARGET\"",
            "git stash \"$ACTION\"",
            "echo before && git branch -D victim",
            "git branch --list -D victim",
        ] {
            let mut gate = Gate {
                allow: true,
                requests: vec![],
            };
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.starts_with("refused:"), "{source}: {out}");
            assert!(
                gate.requests.is_empty(),
                "unresolved policy must not be approvable: {source}"
            );
            assert_eq!(git(repo.path(), &["show-ref"]), before, "{source}");
            assert_eq!(
                git(repo.path(), &["symbolic-ref", "--short", "HEAD"]).trim(),
                "task"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_native_branch_creation_switching_and_queries_keep_git_behavior() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/native-fixture.git",
            ],
        );
        git(repo.path(), &["config", "native.fixture", "configured"]);
        let main_before = git(repo.path(), &["rev-parse", "refs/heads/main"]);
        let mut gate = Gate {
            allow: false,
            requests: vec![],
        };
        for source in [
            "git branch 'native-created' HEAD",
            "git switch native-created",
            "git switch -c native-switched",
            "git checkout -b native-checkout",
            "git checkout -bfeature-attached-checkout",
            "git switch -cfeature-attached-switch",
        ] {
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(!out.contains("refused:"), "{source}: {out}");
        }
        for branch in [
            "native-created",
            "native-switched",
            "native-checkout",
            "feature-attached-checkout",
            "feature-attached-switch",
        ] {
            assert_eq!(
                git(repo.path(), &["rev-parse", &format!("refs/heads/{branch}")]),
                main_before
            );
        }
        assert_eq!(
            git(repo.path(), &["symbolic-ref", "--short", "HEAD"]).trim(),
            "feature-attached-switch"
        );
        for (source, expected) in [
            (
                "git remote -v",
                "https://example.invalid/native-fixture.git",
            ),
            ("git config --get native.fixture", "configured"),
            ("git status --short --branch", "feature-attached-switch"),
            ("git reflog show -1 --format=%gs", "moving from"),
            ("git symbolic-ref --short HEAD", "feature-attached-switch"),
        ] {
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.contains(expected), "{source}: {out}");
            assert!(!out.contains("refused:"), "{source}: {out}");
        }
        assert!(
            gate.requests.is_empty(),
            "ordinary commands must not trigger destructive confirmation"
        );
        assert_eq!(
            git(repo.path(), &["rev-parse", "refs/heads/main"]),
            main_before
        );
    }

    #[tokio::test]
    async fn quoted_native_commit_preserves_head_and_staged_work() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let repo = repository();
        std::fs::write(repo.path().join("pending.txt"), "pending work\n").unwrap();
        git(repo.path(), &["add", "pending.txt"]);
        let before = git(repo.path(), &["rev-parse", "HEAD"]);
        for source in [
            "git 'commit' --no-gpg-sign -m 'must not publish'",
            "'git' commit --no-gpg-sign -m 'must not publish'",
        ] {
            let out = run(source, repo.path(), None).await;
            assert!(out.contains("attribution"), "{source}: {out}");
            assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), before);
            assert_eq!(
                git(repo.path(), &["diff", "--cached", "--name-only"]).trim(),
                "pending.txt"
            );
        }
        for verb in ["merge", "rebase", "cherry-pick", "revert"] {
            assert!(preflight(
                &format!("git '{verb}' HEAD"),
                repo.path(),
                &Caveats::top(),
                &mut None,
                false
            )
            .unwrap_err()
            .contains("attribution"));
            for flag in ["--abort", "--quit"] {
                assert!(preflight(
                    &format!("git '{verb}' '{flag}'"),
                    repo.path(),
                    &Caveats::top(),
                    &mut None,
                    false
                )
                .is_ok());
            }
        }
    }

    #[tokio::test]
    async fn approved_native_deletion_changes_only_requested_branch_or_stash() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let out = run("git branch -D 'victim'", repo.path(), Some(&mut gate)).await;
        assert!(out.contains("Deleted branch victim"), "{out}");
        assert!(!git(repo.path(), &["branch", "--list", "victim"]).contains("victim"));
        assert_eq!(gate.requests.len(), 1);
        assert_eq!(gate.requests[0].target, "branch-delete");
        assert!(
            git(repo.path(), &["show-ref", "--verify", "refs/heads/main"])
                .contains("refs/heads/main")
        );
        let out = run("git stash drop 'stash@{0}'", repo.path(), Some(&mut gate)).await;
        assert!(out.contains("Dropped stash@{0}"), "{out}");
        assert!(git(repo.path(), &["stash", "list"]).is_empty());
        assert_eq!(gate.requests.len(), 2);
        assert_eq!(gate.requests[1].target, "stash-drop");
    }
}

/// issue-1188: real git + real tempdir repos + a real kernel fence, per the
/// workspace's real-resource testing tier. Grounds the broker's plan, the
/// confined copy, the dial (traced at a local smart-HTTP endpoint from the
/// broker's own request) and every F6 sink against the actual processes.
#[cfg(all(test, unix))]
mod governed_push_tests {
    use super::*;
    use crate::caveats::Scope;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;

    /// A write root nothing in these fixtures lives under, so every trust
    /// check sees real (not model-writable) paths.
    const NO_WRITE_ROOT: &str = "/nonexistent/newt-2641-write-root";
    /// An unregistered, non-URL secret the fake helper prints on stderr.
    const CANARY: &str = "NEWT-2641-CANARY-4f9e1c";

    /// Full authority except: net narrowed to github.com (F5 refuses
    /// `Scope::All`) and fs_write narrowed away from the fixtures (F1 refuses
    /// anything model-writable).
    fn scoped_caveats() -> Caveats {
        Caveats {
            fs_write: Scope::only([NO_WRITE_ROOT.to_string()]),
            net: Scope::only(["github.com".to_string()]),
            ..Caveats::top()
        }
    }

    fn with_net(net: Scope<String>) -> Caveats {
        Caveats {
            net,
            ..scoped_caveats()
        }
    }

    fn read_only(roots: &[&Path]) -> Caveats {
        Caveats {
            fs_read: Scope::only(roots.iter().map(|p| p.to_string_lossy().into_owned())),
            ..scoped_caveats()
        }
    }

    /// Tests that reach the confined steps need a kernel fence; elsewhere
    /// the broker refuses (`Unavailable::NoConfinement`) and they skip.
    fn fence() -> bool {
        let ok = crate::confined_exec::kernel_fs_fence_available();
        if !ok {
            eprintln!("skip: no kernel fs fence on this host");
        }
        ok
    }

    /// `tempfile::tempdir()` honours the umask (0775 under this host's
    /// 002); trust-checked fixtures need an owner-only directory.
    fn tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = crate::git_hardening::hardened_git(cwd, args)
            .unwrap()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(cwd: &Path, file: &str, text: &str) -> String {
        std::fs::write(cwd.join(file), text).unwrap();
        git(cwd, &["add", file]);
        git(
            cwd,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-qm",
                text,
            ],
        );
        git(cwd, &["rev-parse", "HEAD"])
    }

    /// Process env for one broker test (process-env lock held): a private
    /// HOME, `PATH` = a fake-gh dir + `/usr/bin:/bin`, `NEWT_CONFIG_DIR` for
    /// the staging state. Restored on drop.
    struct BrokerEnv {
        saved: Vec<(&'static str, Option<String>)>,
        home: tempfile::TempDir,
        _bin: tempfile::TempDir,
        _state: tempfile::TempDir,
        _lock: crate::process_env::EnvGuard,
    }

    impl BrokerEnv {
        fn new() -> Self {
            let lock = crate::process_env::lock();
            let home = tempdir();
            let bin = tempdir();
            let state = tempdir();
            let newt_dir = state.path().join("newt");
            std::fs::create_dir(&newt_dir).unwrap();
            std::fs::set_permissions(&newt_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            // The fake gh answers both the credential-helper protocol and
            // `pr create`, and prints the canary on stderr every time.
            let gh = bin.path().join("gh");
            std::fs::write(
                &gh,
                format!(
                    "#!/bin/sh\necho '{CANARY}' >&2\n\
                     case \"$1 $2\" in\n\
                     'auth git-credential') [ \"$3\" = get ] && printf 'username=u\\npassword=p\\n'; exit 0;;\n\
                     'pr create') echo 'https://github.com/o/r/pull/7'; exit 0;;\n\
                     esac\nexit 1\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let keys = [
                "HOME",
                "PATH",
                "NEWT_CONFIG_DIR",
                "GH_CONFIG_DIR",
                "GIT_DIR",
                "GIT_CONFIG_PARAMETERS",
            ];
            let saved = keys.map(|k| (k, std::env::var(k).ok())).to_vec();
            crate::process_env::set_var("HOME", &home.path().to_string_lossy());
            crate::process_env::set_var("PATH", &format!("{}:/usr/bin:/bin", bin.path().display()));
            crate::process_env::set_var("NEWT_CONFIG_DIR", &newt_dir.to_string_lossy());
            for k in ["GH_CONFIG_DIR", "GIT_DIR", "GIT_CONFIG_PARAMETERS"] {
                crate::process_env::remove_var(k);
            }
            Self {
                saved,
                home,
                _bin: bin,
                _state: state,
                _lock: lock,
            }
        }

        /// `git config --global …` in this HOME, 0600 afterwards.
        fn global(&self, args: &[&str]) {
            let out = std::process::Command::new("/usr/bin/git")
                .args(["config", "--global"])
                .args(args)
                .env_clear()
                .env("HOME", self.home.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            std::fs::set_permissions(
                self.home.path().join(".gitconfig"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
    }

    impl Drop for BrokerEnv {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                crate::process_env::set_or_remove(k, v.as_deref());
            }
        }
    }

    struct Gate {
        allow: bool,
        requests: Vec<PermissionRequest>,
    }

    impl PermissionGate for Gate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.requests.extend_from_slice(requests);
            if self.allow {
                PermissionDecision::Allow(Caveats::top())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_question(&mut self, _question: &str) -> crate::agentic::HumanQuestionOutcome {
            crate::agentic::HumanQuestionOutcome::Unavailable
        }
    }

    /// A repo on `task` (one commit past `main`) with a github.com `origin`.
    fn repo_on_feature_branch() -> tempfile::TempDir {
        let repo = tempdir();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        commit(repo.path(), "f.txt", "one\n");
        git(
            repo.path(),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        git(repo.path(), &["checkout", "-q", "-b", "task"]);
        commit(repo.path(), "f.txt", "two\n");
        repo
    }

    // -- a local smart-HTTP receive-pack endpoint ---------------------------

    fn pkt(data: &[u8]) -> Vec<u8> {
        let mut out = format!("{:04x}", data.len() + 4).into_bytes();
        out.extend_from_slice(data);
        out
    }

    /// What the endpoint saw: every request line (with whether it carried
    /// credentials), and the ref-update command of the receive-pack POST —
    /// the ACTUAL broker's destination path, refspec destination and oid.
    #[derive(Debug, Default)]
    struct Seen {
        requests: Vec<String>,
        update: Option<String>,
    }

    fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => buf.push(byte[0]),
                _ => break,
            }
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let lower = head.to_ascii_lowercase();
        if lower.contains("\r\nexpect: 100-continue") {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let len = lower
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
        (head, body)
    }

    fn respond(stream: &mut TcpStream, status: &str, extra: &str, kind: &str, body: &[u8]) {
        let head = format!(
            "HTTP/1.1 {status}\r\n{extra}Content-Type: {kind}\r\nContent-Length: {}\r\n\
             Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    }

    /// Serve git's smart-HTTP push protocol on 127.0.0.1 until the
    /// receive-pack POST (or a `QUIT` from [`stop`]). With `require_auth`,
    /// an unauthenticated request gets `401`, so git must run its
    /// credential helper.
    fn receive_pack_endpoint(require_auth: bool) -> (u16, std::thread::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Seen::default();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (head, body) = read_request(&mut stream);
                let line = head.lines().next().unwrap_or("").to_string();
                if line.starts_with("QUIT") {
                    break;
                }
                let authed = head.to_ascii_lowercase().contains("\r\nauthorization:");
                seen.requests.push(format!("{line} auth={authed}"));
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                if require_auth && !authed {
                    respond(
                        &mut stream,
                        "401 Unauthorized",
                        "WWW-Authenticate: Basic realm=\"t\"\r\n",
                        "text/plain",
                        b"",
                    );
                } else if path.starts_with("/o/r.git/info/refs?service=git-receive-pack") {
                    let mut adv = pkt(b"# service=git-receive-pack\n");
                    adv.extend_from_slice(b"0000");
                    adv.extend(pkt(format!(
                        "{} capabilities^{{}}\0report-status\n",
                        "0".repeat(40)
                    )
                    .as_bytes()));
                    adv.extend_from_slice(b"0000");
                    respond(
                        &mut stream,
                        "200 OK",
                        "",
                        "application/x-git-receive-pack-advertisement",
                        &adv,
                    );
                } else if path == "/o/r.git/git-receive-pack" {
                    let len = std::str::from_utf8(&body[..4.min(body.len())])
                        .ok()
                        .and_then(|h| usize::from_str_radix(h, 16).ok())
                        .unwrap_or(0);
                    let command = body.get(4..len).unwrap_or_default();
                    let command = command.split(|b| *b == 0).next().unwrap_or_default();
                    let update = String::from_utf8_lossy(command).trim().to_string();
                    let refname = update.rsplit(' ').next().unwrap_or("").to_string();
                    let mut res = pkt(b"unpack ok\n");
                    res.extend(pkt(format!("ok {refname}\n").as_bytes()));
                    res.extend_from_slice(b"0000");
                    respond(
                        &mut stream,
                        "200 OK",
                        "",
                        "application/x-git-receive-pack-result",
                        &res,
                    );
                    seen.update = Some(update);
                    break;
                } else {
                    respond(&mut stream, "404 Not Found", "", "text/plain", b"");
                }
            }
            seen
        });
        (port, handle)
    }

    fn stop(port: u16, handle: std::thread::JoinHandle<Seen>) -> Seen {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"QUIT\r\n\r\n");
        }
        handle.join().expect("endpoint thread")
    }

    // -- planning refusals (no confined step reached) ----------------------

    #[test]
    fn main_branch_push_is_refused() {
        let _env = BrokerEnv::new();
        let repo = tempdir();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        commit(repo.path(), "f.txt", "one\n");
        git(
            repo.path(),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        let err = plan_governed_push(
            "git push origin main:main",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("default branch"), "{err}");
    }

    #[test]
    fn force_push_forms_are_refused() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        for source in [
            "git push --force origin task:task",
            "git push origin task:task --force-with-lease",
            "git push -f origin task:task",
        ] {
            let err =
                plan_governed_push(source, repo.path(), &scoped_caveats(), &mut None).unwrap_err();
            assert!(err.contains("no flags"), "{source}: {err}");
        }
    }

    #[test]
    fn file_and_ext_remotes_are_refused() {
        let _env = BrokerEnv::new();
        for url in ["file:///home/op/other-repo", "ext::sh -c evil"] {
            let repo = repo_on_feature_branch();
            git(repo.path(), &["remote", "set-url", "origin", url]);
            let err = plan_governed_push(
                "git push origin task:task",
                repo.path(),
                &scoped_caveats(),
                &mut None,
            )
            .unwrap_err();
            assert!(
                err.contains("unsupported remote URL scheme"),
                "{url}: {err}"
            );
        }
    }

    #[test]
    fn refspec_operand_naming_a_different_branch_is_refused() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let err = plan_governed_push(
            "git push origin other:other",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn governed_push_requires_exec_and_read_authority() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let no_exec = Caveats {
            exec: Scope::only([]),
            ..scoped_caveats()
        };
        let err = plan_governed_push("git push", repo.path(), &no_exec, &mut None).unwrap_err();
        assert!(err.contains("exec authority"), "{err}");
        let err =
            plan_governed_push("git push", repo.path(), &read_only(&[]), &mut None).unwrap_err();
        assert!(err.contains("read authority"), "{err}");
    }

    /// Read authority is a ROOT: a cwd in a subdirectory of the granted
    /// workspace is inside it. cb241800 used the exact-match
    /// `permits_fs_read`, refusing every subdirectory.
    #[test]
    fn a_subdirectory_cwd_inside_the_read_roots_is_accepted() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let plan =
            plan_governed_push("git push", &sub, &read_only(&[repo.path()]), &mut None).unwrap();
        assert_eq!(plan.branch, "task");
    }

    /// Admin-dir authority is checked BEFORE anything inside is read: a
    /// linked worktree inside the read roots whose administrative directory
    /// (in the main repository) is not, refuses.
    #[test]
    fn a_linked_worktree_whose_admin_dir_is_outside_read_authority_is_refused() {
        let _env = BrokerEnv::new();
        let main = repo_on_feature_branch();
        let outer = tempdir();
        let wt = outer.path().join("wt");
        git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                &wt.to_string_lossy(),
                "-b",
                "wt-task",
            ],
        );
        let err = plan_governed_push("git push", &wt, &read_only(&[outer.path()]), &mut None)
            .unwrap_err();
        assert!(err.contains("read authority"), "{err}");
    }

    /// F2 "authenticate before ANY planning subprocess": a `git` planted
    /// first on PATH inside a write root is refused by the trust check, and
    /// it never runs — not even for `--exec-path` or the remote lookup.
    #[test]
    fn a_planted_git_on_path_refuses_before_any_planning_subprocess() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch(); // fixtures use the real git
        let planted = tempdir();
        let sentinel = planted.path().join("planted-git-ran");
        let git_path = planted.path().join("git");
        std::fs::write(
            &git_path,
            format!(
                "#!/bin/sh\ntouch '{}'\nexec /usr/bin/git \"$@\"\n",
                sentinel.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::process_env::set_var(
            "PATH",
            &format!("{}:/usr/bin:/bin", planted.path().display()),
        );
        let caveats = Caveats {
            fs_write: Scope::only([planted.path().to_string_lossy().into_owned()]),
            ..scoped_caveats()
        };
        let err = plan_governed_push("git push", repo.path(), &caveats, &mut None).unwrap_err();
        assert!(err.contains("model-writable"), "{err}");
        assert!(!sentinel.exists(), "the planted git must never run");
    }

    #[test]
    fn delegated_push_is_refused_by_ordinary_preflight() {
        let source = "timeout 5 git push origin task:task";
        assert!(!needs_push_broker(source), "{source}");
        let err = preflight(source, Path::new("."), &Caveats::top(), &mut None, false);
        assert!(err.is_err(), "{source}: {err:?}");
    }

    // -- F3: the plan pins and approves a verified commit ------------------

    /// F3 approval: the plan carries the commit read at PLAN time, verified
    /// as a commit, and the operator's prompt names it.
    #[test]
    fn the_approval_prompt_names_the_pinned_commit() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let tip = git(repo.path(), &["rev-parse", "task"]);
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let plan = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(plan.oid, tip);
        assert_eq!(
            (plan.url.as_str(), plan.owner.as_str(), plan.name.as_str()),
            ("https://github.com/o/r.git", "o", "r")
        );
        assert_eq!(gate.requests.len(), 1);
        assert_eq!(gate.requests[0].target, "github.com");
        assert!(
            gate.requests[0].reason.contains(&tip),
            "{:?}",
            gate.requests
        );

        let bare =
            plan_governed_push("git push", repo.path(), &scoped_caveats(), &mut None).unwrap();
        assert_eq!(bare, plan);
    }

    #[test]
    fn a_denied_prompt_refuses() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let mut gate = Gate {
            allow: false,
            requests: vec![],
        };
        let err = plan_governed_push(
            "git push",
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        )
        .unwrap_err();
        assert!(err.contains("did not grant"), "{err}");
    }

    /// Finding 6: an `Allow` that does not cover the host is not a yes.
    #[test]
    fn allow_decision_that_does_not_cover_the_host_is_refused() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        struct WrongHostGate;
        impl PermissionGate for WrongHostGate {
            fn ask(&mut self, _requests: &[PermissionRequest]) -> PermissionDecision {
                PermissionDecision::Allow(Caveats {
                    net: Scope::only(["other.example".to_string()]),
                    ..Caveats::top()
                })
            }
            fn ask_question(&mut self, _question: &str) -> crate::agentic::HumanQuestionOutcome {
                crate::agentic::HumanQuestionOutcome::Unavailable
            }
        }
        let err = plan_governed_push(
            "git push",
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut WrongHostGate),
        )
        .unwrap_err();
        assert!(err.contains("does not cover"), "{err}");
    }

    /// F3 type check: a branch ref naming a BLOB is refused at plan time.
    #[test]
    fn a_branch_pointing_at_a_non_commit_is_refused() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let blob = git(repo.path(), &["rev-parse", "HEAD:f.txt"]);
        std::fs::write(
            repo.path().join(".git/refs/heads/task"),
            format!("{blob}\n"),
        )
        .unwrap();
        let err =
            plan_governed_push("git push", repo.path(), &scoped_caveats(), &mut None).unwrap_err();
        assert!(err.contains("not a commit"), "{err}");
    }

    /// A repo-local config gadget planted in the workspace has NO effect on
    /// planning: nothing reads `.git/config` except the literal remote url.
    #[test]
    fn hostile_repo_local_config_has_no_effect_on_the_plan() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let sentinel = repo.path().join("payload-ran");
        git(
            repo.path(),
            &[
                "config",
                "--local",
                "core.sshCommand",
                &format!("touch {}", sentinel.display()),
            ],
        );
        let plan =
            plan_governed_push("git push", repo.path(), &scoped_caveats(), &mut None).unwrap();
        assert_eq!(plan.url, "https://github.com/o/r.git");
        assert!(!sentinel.exists());
    }

    // -- the real broker against a local receive-pack endpoint -------------

    /// Plan against the github.com origin, then point the APPROVED plan at a
    /// local endpoint (the only field changed) so the real broker dials it.
    fn plan_for_endpoint(cwd: &Path, caveats: &Caveats, port: u16) -> GovernedPushPlan {
        let mut plan = plan_governed_push("git push", cwd, caveats, &mut None).unwrap();
        plan.url = format!("http://127.0.0.1:{port}/o/r.git");
        plan
    }

    /// F3 regression + destination proof, traced at the endpoint from the
    /// ACTUAL broker's request: after approval the workspace branch moves,
    /// and the broker still pushes the approved commit, to the approved
    /// path. Hostile inherited `GIT_DIR`/`GIT_CONFIG_PARAMETERS` and a
    /// test-HOME global `pushInsteadOf` all try to redirect it; none reach
    /// the child. At cb241800 the oid was read AFTER approval.
    #[test]
    fn the_broker_pushes_the_approved_oid_after_the_branch_moves() {
        if !fence() {
            return;
        }
        let env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let (port, endpoint) = receive_pack_endpoint(false);
        let approved = git(repo.path(), &["rev-parse", "task"]);
        let plan = plan_for_endpoint(repo.path(), &scoped_caveats(), port);

        let moved = commit(repo.path(), "f.txt", "three\n");
        assert_ne!(moved, approved);
        let decoy = "http://127.0.0.1:1/";
        env.global(&[
            &format!("url.{decoy}.pushInsteadOf"),
            &format!("http://127.0.0.1:{port}/"),
        ]);
        crate::process_env::set_var("GIT_DIR", "/nonexistent/hostile-git-dir");
        crate::process_env::set_var(
            "GIT_CONFIG_PARAMETERS",
            &format!("'url.{decoy}.pushinsteadof'='http://127.0.0.1:{port}/'"),
        );

        let outcome = run_staged_push(&plan, &scoped_caveats());
        let seen = stop(port, endpoint);
        assert_eq!(
            seen.update.as_deref(),
            Some(format!("{} {approved} refs/heads/task", "0".repeat(40)).as_str()),
            "{seen:?} / {outcome:?}"
        );
        assert!(
            seen.requests
                .iter()
                .any(|r| r.starts_with("POST /o/r.git/git-receive-pack ")),
            "{seen:?}"
        );
        assert_eq!(
            outcome.unwrap().to_string(),
            format!("pushed {approved} → github.com/o/r:task")
        );
    }

    /// F4 positive proof: a linked worktree (admin dir in the main repo's
    /// `worktrees/`, objects in its common dir) is copied by the confined
    /// fetch and pushed, under read authority limited to the two trees.
    #[test]
    fn a_linked_worktree_push_is_copied_confined_and_pushed() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let root = tempdir();
        let main = root.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        commit(&main, "f.txt", "one\n");
        git(
            &main,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        let wt = root.path().join("wt");
        git(
            &main,
            &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "task"],
        );
        let tip = commit(&wt, "g.txt", "wt\n");

        let caveats = read_only(&[root.path()]);
        let (port, endpoint) = receive_pack_endpoint(false);
        let plan = plan_for_endpoint(&wt, &caveats, port);
        let outcome = run_staged_push(&plan, &caveats);
        let seen = stop(port, endpoint);
        assert_eq!(
            seen.update.as_deref(),
            Some(format!("{} {tip} refs/heads/task", "0".repeat(40)).as_str()),
            "{seen:?} / {outcome:?}"
        );
    }

    /// F4 fence proof. The workspace borrows its objects through an
    /// alternate. Positive control first: with the alternate inside the read
    /// roots the push goes through. Then the alternate is swapped, after
    /// planning (the last check), to an identical copy OUTSIDE the roots:
    /// the confined copy cannot read it, the broker refuses, and the
    /// endpoint sees no request at all.
    #[test]
    fn a_swapped_alternate_outside_the_read_roots_makes_the_copy_fail() {
        if !fence() {
            return;
        }
        let _env = BrokerEnv::new();
        let root = tempdir();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        commit(&source, "f.txt", "one\n");
        let ws = root.path().join("ws");
        git(
            root.path(),
            &[
                "clone",
                "-q",
                "--shared",
                &source.to_string_lossy(),
                &ws.to_string_lossy(),
            ],
        );
        git(
            &ws,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );
        git(&ws, &["checkout", "-q", "-b", "task"]);
        let caveats = read_only(&[root.path()]);

        let (port, endpoint) = receive_pack_endpoint(false);
        let plan = plan_for_endpoint(&ws, &caveats, port);
        let ok = run_staged_push(&plan, &caveats);
        let seen = stop(port, endpoint);
        assert!(seen.update.is_some(), "positive control: {seen:?} / {ok:?}");

        let (port, endpoint) = receive_pack_endpoint(false);
        let plan = plan_for_endpoint(&ws, &caveats, port);
        let outside = tempdir();
        let copy = std::process::Command::new("cp")
            .arg("-a")
            .arg(source.join(".git/objects"))
            .arg(outside.path().join("objects"))
            .status()
            .unwrap();
        assert!(copy.success());
        std::fs::write(
            ws.join(".git/objects/info/alternates"),
            format!("{}\n", outside.path().join("objects").display()),
        )
        .unwrap();
        let refused = run_staged_push(&plan, &caveats);
        let seen = stop(port, endpoint);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains("confined copy")),
            "{refused:?}"
        );
        assert!(seen.requests.is_empty(), "{seen:?}");
    }

    // -- gh pr create planning ----------------------------------------------

    #[test]
    fn granted_host_plans_the_exact_fixed_pr_create() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let plan = plan_governed_pr_create(
            "gh pr create --title 't' --body 'b'",
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(
            (
                plan.repo.as_str(),
                plan.base.as_str(),
                plan.head.as_str(),
                plan.title.as_str(),
                plan.body.as_str()
            ),
            ("o/r", "main", "task", "t", "b")
        );
        assert_eq!(gate.requests[0].target, "github.com");
    }

    #[test]
    fn unsupported_pr_create_flags_are_refused() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b --base other",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn non_github_remote_is_refused_for_pr_create() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        git(
            repo.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "https://gitlab.example.com/o/r.git",
            ],
        );
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("github.com"), "{err}");
    }

    #[test]
    fn pr_create_plan_is_unaffected_by_a_hostile_repo_local_config() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        git(
            repo.path(),
            &[
                "config",
                "--local",
                "core.sshCommand",
                "ssh -oProxyCommand=evil",
            ],
        );
        let plan = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap();
        assert_eq!(plan.repo, "o/r");
    }

    #[test]
    fn pr_create_requires_exec_and_read_authority() {
        let _env = BrokerEnv::new();
        let repo = repo_on_feature_branch();
        let no_exec = Caveats {
            exec: Scope::only([]),
            ..scoped_caveats()
        };
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &no_exec,
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("exec authority"), "{err}");
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &read_only(&[]),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("read authority"), "{err}");
    }
}
