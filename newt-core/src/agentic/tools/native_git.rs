//! Transitional native Git checks, not a replacement for a repository broker.
//! Keep the caller's shell source unchanged and preserve the embedded adapter's
//! destructive-operation confirmations while the command interface migrates.

use super::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use crate::caveats::{Caveats, CaveatsExt};
use crate::git_caveats::GitCaveats;
use agent_bridle::{inspect_shell, ShellInspection};
use std::path::{Path, PathBuf};
use std::process::Command;

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
///
/// Unlike the earlier "credentialed_git in the workspace" mechanism, nothing
/// git executes here ever reads the workspace's `.git/config` — see
/// [`crate::git_staging`]'s module doc for the six-phase staging mechanism.
/// The model only ever sees the fixed-form [`crate::git_staging::Outcome`]
/// string (F6): raw child stdout/stderr is captured and dropped.
pub(super) fn execute_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let plan = plan_governed_push(source, cwd, caveats, gate)?;
    let outcome = run_staged_push(&plan, cwd, caveats)?;
    Ok(outcome.to_string())
}

/// Everything [`execute_governed_push`] must verify BEFORE it may create a
/// staging repo at all: argv shape, main/master refusal, the literal
/// destination URL and its scheme, and the net-gate prompt. Split out from
/// [`execute_governed_push`] so the test suite can pin every refusal and the
/// exact plan a granted happy path resolves to without needing a real
/// reachable forge — the same boundary amendment A4 already draws for `gh pr
/// create`'s live-forge exercise.
fn plan_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<GovernedPushPlan, String> {
    crate::git_staging::preflight_availability(caveats).map_err(|e| e.to_string())?;

    let argv = standalone_literal_argv(source)?;
    let request = parse_governed_push(&argv)?;

    // Finding 6 (issue-1188 review #2641): this broker runs entirely
    // outside the confined shell's filesystem/exec fence, so it must bind
    // itself to the SAME authority a confined command would have needed —
    // read access to the repository it is about to operate on, and exec
    // authority for `git` itself. Without this, a model-chosen `cwd` naming
    // some OTHER operator repository (one the confined fence would have
    // refused to even read) reaches a real host-side git process.
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

    let branch = crate::git_hardening::own_branch(cwd)
        .ok_or_else(|| "refused: detached HEAD has no branch to push".to_string())?;
    if crate::git_hardening::is_default_branch(cwd, &branch) {
        return Err(format!(
            "refused: cannot push the default branch '{branch}' (open a feature branch instead)"
        ));
    }
    // Finding 2: an explicit `<remote> <branch>:<branch>` operand must name
    // the SAME branch this broker is about to push — never silently
    // substituted, and never itself used to build the refspec below.
    if let Some(requested) = &request.requested_branch {
        if requested != &branch {
            return Err(format!(
                "refused: requested branch '{requested}' does not match the checked-out \
                 branch '{branch}' — a governed push always pushes the workspace's own branch"
            ));
        }
    }

    // A2 + staging design P1: the LITERAL declared value of
    // `remote.<name>.url`, never `git remote get-url` (which applies
    // `insteadOf`/`pushInsteadOf` — DESIGN-r4 probe P1: that rewrite survives
    // every `-c`/env override, so reading through it at all is the wrong
    // primitive). This exact string is BOTH what the operator is shown and
    // what staging later dials — staging has no `url.*` keys to rewrite it a
    // second time, which is what closes the approve/dial bait-and-switch.
    let url = crate::git_staging::literal_remote_url(cwd, &request.remote)?;
    let host = crate::git_hardening::push_url_host(&url)?;
    let Some((owner, name)) = crate::git_hardening::github_owner_repo(&url) else {
        return Err(format!(
            "refused: a governed push is only supported for a github.com remote (got {url})"
        ));
    };

    ensure_net_granted(
        caveats,
        gate,
        &host,
        &format!("push branch '{branch}' to {host} ({url})"),
    )?;

    Ok(GovernedPushPlan {
        url,
        owner,
        name,
        branch,
    })
}

/// The exact push [`execute_governed_push`] will stage and dial, resolved and
/// net-gated but not yet spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GovernedPushPlan {
    url: String,
    owner: String,
    name: String,
    branch: String,
}

/// Phases 2-6 of the staging-repo broker (`crate::git_staging`) for a push
/// already planned and net-gated by [`plan_governed_push`]. Returns a fixed-
/// form [`crate::git_staging::Outcome`] — never the raw child output (F6).
fn run_staged_push(
    plan: &GovernedPushPlan,
    cwd: &Path,
    caveats: &Caveats,
) -> Result<crate::git_staging::Outcome, String> {
    use crate::git_staging::{self, FailureCategory, Outcome};

    if !crate::confined_exec::kernel_fs_fence_available() {
        return Err(git_staging::Unavailable::NoConfinement.to_string());
    }

    let (common_dir, git_dir) = resolve_git_dirs_trusted(cwd, caveats)?;
    let oid = git_staging::read_branch_oid(&git_dir, &plan.branch)
        .or_else(|_| git_staging::read_branch_oid(&common_dir, &plan.branch))?;

    let objects_dir = common_dir.join("objects");
    let chain = git_staging::resolve_alternates_chain(&objects_dir, &caveats.fs_read)?;
    let first = chain
        .first()
        .cloned()
        .ok_or_else(|| "refused: could not resolve workspace objects directory".to_string())?;

    let path = std::env::var_os("PATH");
    let git_bin =
        crate::git_hardening::trusted_git_program(path.as_deref()).map_err(|e| e.to_string())?;
    let gh_bin =
        crate::git_hardening::trusted_gh_program(path.as_deref()).map_err(|e| e.to_string())?;
    git_staging::trust_check(&git_bin, &caveats.fs_write)?;
    git_staging::trust_check(&gh_bin, &caveats.fs_write)?;
    let git_bin_dir = git_bin
        .parent()
        .ok_or_else(|| "refused: git binary has no parent directory".to_string())?;
    let gh_bin_dir = gh_bin
        .parent()
        .ok_or_else(|| "refused: gh binary has no parent directory".to_string())?;
    git_staging::trust_check(git_bin_dir, &caveats.fs_write)?;
    git_staging::trust_check(gh_bin_dir, &caveats.fs_write)?;

    let state_dir = crate::config::Config::user_config_dir()
        .ok_or_else(|| "refused: no resolvable state directory for a staging repo".to_string())?
        .join("staging");
    let staging = git_staging::StagingRepo::create(&state_dir, &caveats.fs_write)?;

    git_staging::import_credentials(&staging, &caveats.fs_write, &gh_bin)?;
    git_staging::write_alternates(&staging, &first)?;

    // Phase 3 (F4): the ONLY step that reads workspace objects runs CONFINED.
    let confined_caveats = Caveats {
        fs_read: narrowed_read_scope(&caveats.fs_read, staging.path()),
        fs_write: crate::caveats::Scope::only([staging.path().to_string_lossy().into_owned()]),
        exec: crate::caveats::Scope::only([git_bin.to_string_lossy().into_owned()]),
        net: crate::caveats::Scope::none(),
        ..caveats.clone()
    };
    let staging_display = staging.path().to_string_lossy().into_owned();
    let first_display = first.to_string_lossy().into_owned();
    let fetch_req = crate::confined_exec::ExecRequest::new(
        crate::confined_exec::ExecOrigin::TrustedInfra,
        git_bin.to_string_lossy().into_owned(),
        [
            "-C",
            staging_display.as_str(),
            "fetch",
            "--no-tags",
            first_display.as_str(),
            oid.as_str(),
        ],
        staging.path().to_path_buf(),
        confined_caveats,
    );
    let fetch_out = crate::confined_exec::ConstrainedExecutor::run(&fetch_req)
        .map_err(|e| format!("refused: confined fetch could not run — {e}"))?;
    if !fetch_out.success {
        return Ok(Outcome::Failed {
            category: FailureCategory::GitError,
        });
    }

    // F4: delete alternates, then prove self-containment BEFORE the network
    // step runs — a nested alternate swapped out from under the earlier
    // check has no effect once this passes, because a missing alternates
    // file cannot resolve anything outside staging's own object store.
    git_staging::remove_alternates(&staging)?;
    let fsck = Command::new(&git_bin)
        .arg("-C")
        .arg(staging.path())
        .args(["fsck", "--connectivity-only", &oid])
        .env_clear()
        .output()
        .map_err(|e| e.to_string())?;
    if !fsck.status.success() {
        return Err(
            "refused: staging repo failed connectivity check after the confined fetch".to_string(),
        );
    }

    let refspec = format!("refs/heads/{}:refs/heads/{}", plan.branch, plan.branch);
    let gh_config_dir = git_staging::trusted_gh_config_dir(&caveats.fs_write)?;
    let env = git_staging::governed_child_env(&[git_bin_dir, gh_bin_dir], gh_config_dir.as_deref());

    let mut cmd = Command::new(&git_bin);
    cmd.arg("-C")
        .arg(staging.path())
        .arg("-c")
        .arg("http.followRedirects=false")
        .args(["push", &plan.url, &refspec])
        .env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(Outcome::Pushed {
            oid,
            owner: plan.owner.clone(),
            name: plan.name.clone(),
            branch: plan.branch.clone(),
        })
    } else {
        Ok(Outcome::Failed {
            category: classify_failure(&output.stderr),
        })
    }
}

/// Follow a linked worktree's `gitdir:` file to the real administrative
/// directory and its commondir (CONDUCTOR-ADDENDUM item 3), verifying BOTH
/// are inside the session's authorized read roots before anything reads a
/// ref from them.
fn resolve_git_dirs_trusted(cwd: &Path, caveats: &Caveats) -> Result<(PathBuf, PathBuf), String> {
    let (common_dir, git_dir) = crate::git_hardening::git_dirs(cwd).ok_or_else(|| {
        "refused: could not resolve this repository's git directories".to_string()
    })?;
    for dir in [&common_dir, &git_dir] {
        if !crate::caveats::permits_path(&caveats.fs_read, &dir.to_string_lossy()) {
            return Err(format!(
                "refused: '{}' is outside this session's filesystem read authority",
                dir.display()
            ));
        }
    }
    Ok((common_dir, git_dir))
}

/// Add `extra` to a read scope without discarding `Scope::All` (there is
/// nothing to narrow when the session already has unrestricted read).
fn narrowed_read_scope(
    fs_read: &crate::caveats::Scope<String>,
    extra: &Path,
) -> crate::caveats::Scope<String> {
    match fs_read {
        crate::caveats::Scope::All => crate::caveats::Scope::All,
        crate::caveats::Scope::Only(set) => {
            let mut v: Vec<String> = set.iter().cloned().collect();
            v.push(extra.to_string_lossy().into_owned());
            crate::caveats::Scope::only(v)
        }
    }
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
/// `.git/config` is ever in scope for `gh`'s own git children), for the same
/// reason as [`execute_governed_push`] (the confined child cannot reach the
/// forge at all). The model only ever sees the fixed-form
/// [`crate::git_staging::Outcome`] string.
pub(super) fn execute_governed_pr_create(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let plan = plan_governed_pr_create(source, cwd, caveats, gate)?;
    let outcome = run_staged_pr_create(&plan, caveats)?;
    Ok(outcome.to_string())
}

/// Phase 2/4/5 for `gh pr create`: a bare staging dir (no objects needed —
/// `gh` never reads repository content to open a PR), `gh`'s own trust-
/// checked config dir, and the fixed-form outcome (F6): a `pr_created`
/// outcome's URL is validated against
/// [`crate::git_staging::validate_pr_url`] or the outcome degrades to
/// `failed(parse)`.
fn run_staged_pr_create(
    plan: &GovernedPrCreatePlan,
    caveats: &Caveats,
) -> Result<crate::git_staging::Outcome, String> {
    use crate::git_staging::{self, FailureCategory, Outcome};

    let path = std::env::var_os("PATH");
    let git_bin =
        crate::git_hardening::trusted_git_program(path.as_deref()).map_err(|e| e.to_string())?;
    let gh_bin =
        crate::git_hardening::trusted_gh_program(path.as_deref()).map_err(|e| e.to_string())?;
    git_staging::trust_check(&git_bin, &caveats.fs_write)?;
    git_staging::trust_check(&gh_bin, &caveats.fs_write)?;
    let git_bin_dir = git_bin
        .parent()
        .ok_or_else(|| "refused: git binary has no parent directory".to_string())?;
    let gh_bin_dir = gh_bin
        .parent()
        .ok_or_else(|| "refused: gh binary has no parent directory".to_string())?;
    git_staging::trust_check(git_bin_dir, &caveats.fs_write)?;
    git_staging::trust_check(gh_bin_dir, &caveats.fs_write)?;

    let state_dir = crate::config::Config::user_config_dir()
        .ok_or_else(|| "refused: no resolvable state directory for a staging repo".to_string())?
        .join("staging");
    let staging = git_staging::StagingRepo::create(&state_dir, &caveats.fs_write)?;
    git_staging::import_credentials(&staging, &caveats.fs_write, &gh_bin)?;

    let gh_config_dir = git_staging::trusted_gh_config_dir(&caveats.fs_write)?;
    let env = git_staging::governed_child_env(&[git_bin_dir, gh_bin_dir], gh_config_dir.as_deref());

    let mut cmd = Command::new(&gh_bin);
    cmd.current_dir(staging.path())
        .args([
            "pr",
            "create",
            "--repo",
            &format!("github.com/{}", plan.repo),
            "--base",
            &plan.base,
            "--head",
            &plan.head,
            "--title",
            &plan.title,
            "--body",
            &plan.body,
        ])
        .env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(Outcome::Failed {
            category: classify_failure(&output.stderr),
        });
    }
    match git_staging::validate_pr_url(&String::from_utf8_lossy(&output.stdout)) {
        Some(url) => Ok(Outcome::PrCreated { url }),
        None => Ok(Outcome::Failed {
            category: FailureCategory::Parse,
        }),
    }
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
}

fn plan_governed_pr_create(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<GovernedPrCreatePlan, String> {
    crate::git_staging::preflight_availability(caveats).map_err(|e| e.to_string())?;

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

    // Unlike the earlier mechanism, a repo-local config gadget has no effect
    // on this broker at all: `gh pr create` now always dials from a fresh
    // staging directory with no workspace `.git/config` in scope (Phase 4),
    // so there is nothing here left to screen for — see
    // `crate::git_staging`'s module doc.
    let url = crate::git_hardening::resolve_remote_url(cwd, "origin")?;
    let Some((owner, name)) = crate::git_hardening::github_owner_repo(&url) else {
        return Err(format!(
            "refused: gh pr create is only supported for a github.com 'origin' remote (got {url})"
        ));
    };
    let host = "github.com".to_string();

    let head = crate::git_hardening::own_branch(cwd)
        .ok_or_else(|| "refused: detached HEAD has no branch to open a PR from".to_string())?;
    let base =
        crate::git_hardening::origin_default_branch_name(cwd).unwrap_or_else(|| "main".to_string());
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

/// issue-1188: real git + real tempdir repos, per the workspace's
/// expensive/real-resource testing tier (small and self-contained enough to
/// run inline, same posture as `git_hardening`'s own real-process tests).
/// One test per DESIGN.md/DESIGN-REVIEW.md contract and amendment.
#[cfg(all(test, unix))]
mod governed_push_tests {
    use super::*;

    /// `Caveats::top()` narrowed to a github.com net grant: unlike
    /// `Caveats::top()`'s bare `Scope::All`, this passes SPEC-FINAL F5's
    /// preflight (an unrestricted net grant disclaims push governance
    /// entirely) while still exercising every other check at full authority
    /// — the shape a governed push actually expects to run under.
    fn scoped_caveats() -> Caveats {
        Caveats {
            net: crate::caveats::Scope::only(["github.com".to_string()]),
            ..Caveats::top()
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
        String::from_utf8_lossy(&output.stdout).into_owned()
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

    /// A repo on `task`, tracking a `https://` `origin` a granted-host test
    /// can "push" against (planning only — see [`plan_governed_push`]'s doc
    /// comment for why no test here dials a real forge).
    fn repo_on_feature_branch() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "one\n").unwrap();
        git(repo.path(), &["add", "f.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-qm",
                "init",
            ],
        );
        git(
            repo.path(),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        git(repo.path(), &["checkout", "-q", "-b", "task"]);
        repo
    }

    /// Would have failed before A2/main-refusal ordering: pushing while on
    /// `main` must be refused before any network authority is even checked.
    #[test]
    fn main_branch_push_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "one\n").unwrap();
        git(repo.path(), &["add", "f.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-qm",
                "init",
            ],
        );
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

    /// Force is not merely checked at runtime — the fixed-argv builder has no
    /// slot for it at all. Every spelling of "force" is refused before the
    /// remote/branch are even resolved.
    #[test]
    fn force_push_forms_are_refused() {
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

    /// DESIGN-r4's finding-closure table, F2 row: a repo-local config gadget
    /// planted in the workspace's `.git/config` has NO EFFECT on the plan at
    /// all — `plan_governed_push` never reads `.git/config` for anything but
    /// `remote.<name>.url` any more (`crate::git_staging::literal_remote_url`,
    /// a literal `git config --get`, not a config-driven resolver a hook
    /// could hijack). Would have failed against the OLD mechanism only in the
    /// sense that it asserted a REFUSAL; the staging design's answer is that
    /// the gadget is simply never consulted, so planning still succeeds.
    #[test]
    fn hostile_repo_local_config_has_no_effect_on_the_plan() {
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
        let plan = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap();
        assert_eq!(plan.url, "https://github.com/o/r.git");
        assert!(
            !sentinel.exists(),
            "a repo-local config gadget must never run just from planning a push"
        );
    }

    /// A `file://` remote (or anything else not `https://`/`ssh://`/scp-like)
    /// is refused by amendment A2, structurally, before the net gate runs.
    #[test]
    fn file_and_ext_remotes_are_refused() {
        for url in ["file:///home/op/other-repo", "ext::sh -c evil"] {
            let repo = tempfile::tempdir().unwrap();
            git(repo.path(), &["init", "-q", "-b", "main"]);
            std::fs::write(repo.path().join("f.txt"), "one\n").unwrap();
            git(repo.path(), &["add", "f.txt"]);
            git(
                repo.path(),
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "commit",
                    "-qm",
                    "init",
                ],
            );
            git(repo.path(), &["remote", "add", "origin", url]);
            git(repo.path(), &["checkout", "-q", "-b", "task"]);
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

    /// An ungranted host is a permission-gate PROMPT (folded into the
    /// existing `net:<host>` vocabulary, amendment A5), not a silent denial —
    /// and a deny leaves the push unexecuted.
    #[test]
    fn ungranted_host_prompts_and_a_denial_refuses() {
        let repo = repo_on_feature_branch();
        let restricted = Caveats {
            net: crate::caveats::Scope::only([]),
            ..Caveats::top()
        };
        let mut gate = Gate {
            allow: false,
            requests: vec![],
        };
        let err = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &restricted,
            &mut Some(&mut gate),
        )
        .unwrap_err();
        assert!(err.contains("did not grant"), "{err}");
        assert_eq!(gate.requests.len(), 1);
        assert_eq!(gate.requests[0].target, "github.com");
    }

    /// The happy path: every structural gate passes and the plan resolves to
    /// the EXACT fixed argv a real push would run — `git remote get-url`'s
    /// answer, not the bare `origin` argument, and `<branch>:<branch>` for
    /// the repository's own checked-out branch. Actually dialing a real
    /// `https://github.com` remote is exercised by the witnessed v0.8.0 gate
    /// run, not this unit-tier test (same scope boundary amendment A4 draws
    /// for `gh pr create`'s live forge).
    #[test]
    fn granted_host_plans_the_exact_fixed_push() {
        let repo = repo_on_feature_branch();
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let plan = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &scoped_caveats(),
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(plan.url, "https://github.com/o/r.git");
        assert_eq!(plan.owner, "o");
        assert_eq!(plan.name, "r");
        assert_eq!(plan.branch, "task");

        // Bare `git push` (no operands) resolves the same way, from origin +
        // the repository's own branch.
        let plan2 =
            plan_governed_push("git push", repo.path(), &scoped_caveats(), &mut None).unwrap();
        assert_eq!(plan2, plan);
    }

    /// Finding 2 (issue-1188 review #2641): a `<remote> <branch>:<branch>`
    /// operand that does NOT name the workspace's actual checked-out branch
    /// must be refused, not silently substituted with the real branch. Would
    /// have failed before the fix: `parse_governed_push` discarded the
    /// operand after checking only `src == dst`, so this returned `Ok` and
    /// silently pushed `task`, never `other`.
    #[test]
    fn refspec_operand_naming_a_different_branch_is_refused() {
        let repo = repo_on_feature_branch(); // checked out on "task"
        let err = plan_governed_push(
            "git push origin other:other",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    /// Finding 6 (issue-1188 review #2641): the broker runs outside the
    /// confined shell's own filesystem/exec fence entirely, so it must
    /// re-check that authority itself. Would have failed before the fix:
    /// neither check existed, so a restricted `Caveats` still reached
    /// `resolve_remote_url`/the net gate.
    #[test]
    fn governed_push_requires_exec_authority_for_git() {
        let repo = repo_on_feature_branch();
        let no_exec = Caveats {
            exec: crate::caveats::Scope::only([]),
            net: crate::caveats::Scope::only(["github.com".to_string()]),
            ..Caveats::top()
        };
        let err = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &no_exec,
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("exec authority"), "{err}");
    }

    #[test]
    fn governed_push_requires_read_authority_for_cwd() {
        let repo = repo_on_feature_branch();
        let no_read = Caveats {
            fs_read: crate::caveats::Scope::only([]),
            net: crate::caveats::Scope::only(["github.com".to_string()]),
            ..Caveats::top()
        };
        let err = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &no_read,
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("read authority"), "{err}");
    }

    /// Read authority is a ROOT: a cwd in a subdirectory of the granted
    /// workspace is inside it. `permits_fs_read` is exact-match (documented
    /// in `CaveatsExt`), so the broker refused every subdirectory cwd.
    #[test]
    fn a_subdirectory_cwd_inside_the_read_roots_is_accepted() {
        let repo = repo_on_feature_branch();
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let caveats = Caveats {
            fs_read: crate::caveats::Scope::only([repo.path().to_string_lossy().into_owned()]),
            ..scoped_caveats()
        };
        let plan = plan_governed_push("git push", &sub, &caveats, &mut None).unwrap();
        assert_eq!(plan.branch, "task");
    }

    /// Finding 6 (issue-1188 review #2641): `PermissionDecision::Allow`'s
    /// returned capability must actually cover the requested host — a gate
    /// that grants a DIFFERENT host must not be read as a yes just because
    /// the variant is `Allow`. Would have failed before the fix:
    /// `ensure_net_granted` discarded the returned `Caveats` entirely.
    #[test]
    fn allow_decision_that_does_not_cover_the_host_is_refused() {
        let repo = repo_on_feature_branch();
        struct WrongHostGate;
        impl PermissionGate for WrongHostGate {
            fn ask(&mut self, _requests: &[PermissionRequest]) -> PermissionDecision {
                PermissionDecision::Allow(Caveats {
                    net: crate::caveats::Scope::only(["other.example".to_string()]),
                    ..Caveats::top()
                })
            }
            fn ask_question(&mut self, _question: &str) -> crate::agentic::HumanQuestionOutcome {
                crate::agentic::HumanQuestionOutcome::Unavailable
            }
        }
        let restricted = Caveats {
            net: crate::caveats::Scope::only([]),
            ..Caveats::top()
        };
        let mut gate = WrongHostGate;
        let err = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &restricted,
            &mut Some(&mut gate),
        )
        .unwrap_err();
        assert!(err.contains("does not cover"), "{err}");
    }

    /// Finding 7 (issue-1188 review #2641): a `git push` reached only through
    /// a descendant dispatcher (never a standalone top-level command) must be
    /// refused outright by ordinary preflight, not silently fall through to
    /// the confined shell where the broker's branch/force guarantees do not
    /// apply. Would have failed before the fix: `push` had no arm in
    /// `inspect_commands`'s verb match, so it hit the `_ => continue`
    /// catch-all and executed normally.
    #[test]
    fn delegated_push_is_refused_by_ordinary_preflight() {
        let source = "timeout 5 git push origin task:task";
        assert!(
            !needs_push_broker(source),
            "a descendant-only push must not (yet) route to the governed broker: {source}"
        );
        let err = preflight(source, Path::new("."), &Caveats::top(), &mut None, false);
        assert!(err.is_err(), "{source}: {err:?}");
    }

    /// SPEC-FINAL F5, through the OUTER tool path (`needs_push_broker` +
    /// `execute_governed_push`, the same route `tools.rs`'s dispatch takes):
    /// under an unrestricted (`Scope::All`) net grant, a top-level `git push`
    /// gets the broker-refused message and no mutation — push governance is
    /// disclaimed entirely rather than pretending to bound an opaque
    /// launcher's descendant network.
    #[test]
    fn f5_top_level_push_under_scope_all_net_is_refused_and_never_mutates() {
        let repo = repo_on_feature_branch();
        let source = "git push origin task:task";
        assert!(
            needs_push_broker(source),
            "must route to the governed broker"
        );
        let caveats = Caveats {
            net: crate::caveats::Scope::All,
            ..Caveats::top()
        };
        let err = execute_governed_push(source, repo.path(), &caveats, &mut None).unwrap_err();
        assert!(
            err.contains("Scope::All") || err.contains("disclaimed"),
            "{err}"
        );
        // No ref moved on the remote-tracking side; the workspace's own
        // branch ref is untouched too (refusal happened before staging).
        let head_after = git(repo.path(), &["rev-parse", "task"]);
        assert!(!head_after.trim().is_empty());
    }

    /// The exact `gh pr create` argv this broker resolves — `--repo` is
    /// ALWAYS present (amendment A4), base/head are resolved by the broker,
    /// never taken from the model's argv.
    #[test]
    fn granted_host_plans_the_exact_fixed_pr_create() {
        let repo = repo_on_feature_branch();
        // `origin/HEAD` unset in this fixture, so the base falls back to the
        // documented "main" default.
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let restricted = Caveats {
            net: crate::caveats::Scope::only([]),
            ..Caveats::top()
        };
        let plan = plan_governed_pr_create(
            "gh pr create --title 't' --body 'b'",
            repo.path(),
            &restricted,
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(plan.repo, "o/r");
        assert_eq!(plan.base, "main");
        assert_eq!(plan.head, "task");
        assert_eq!(plan.title, "t");
        assert_eq!(plan.body, "b");
        assert_eq!(gate.requests[0].target, "github.com");
    }

    #[test]
    fn unsupported_flags_are_refused() {
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
    fn non_github_remote_is_refused() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = crate::git_hardening::hardened_git(repo.path(), args)
                .unwrap()
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "one\n").unwrap();
        git(&["add", "f.txt"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "init",
        ]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://gitlab.example.com/o/r.git",
        ]);
        git(&["checkout", "-q", "-b", "task"]);
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &scoped_caveats(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("github.com"), "{err}");
    }

    /// DESIGN-r4 finding closure, PR-create side: the actual `gh pr create`
    /// dial runs from a fresh staging directory (Phase 4), so a repo-local
    /// config gadget has no way to reach it — planning still succeeds.
    #[test]
    fn pr_create_plan_is_unaffected_by_a_hostile_repo_local_config() {
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

    /// Finding 6 (issue-1188 review #2641): the PR-create broker must bind
    /// itself to the same authority the push broker does.
    #[test]
    fn pr_create_requires_exec_and_read_authority() {
        let repo = repo_on_feature_branch();
        let no_exec = Caveats {
            exec: crate::caveats::Scope::only([]),
            net: crate::caveats::Scope::only(["github.com".to_string()]),
            ..Caveats::top()
        };
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &no_exec,
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("exec authority"), "{err}");

        let no_read = Caveats {
            fs_read: crate::caveats::Scope::only([]),
            net: crate::caveats::Scope::only(["github.com".to_string()]),
            ..Caveats::top()
        };
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &no_read,
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("read authority"), "{err}");
    }
}
