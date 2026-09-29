//! Transitional native Git checks, not a replacement for a repository broker.
//! Keep the caller's shell source unchanged and preserve the embedded adapter's
//! destructive-operation confirmations while the command interface migrates.

use super::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use crate::caveats::{Caveats, CaveatsExt};
use crate::git_caveats::GitCaveats;
use agent_bridle::{inspect_shell, ShellInspection};
use std::path::Path;

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

/// issue-1188: execute a governed `git push`, entirely host-side (never
/// through the confined shell — see the call site's comment in `tools.rs`).
/// `cwd` is the resolved directory the model's `run_command` targeted (after
/// any folded leading `cd`); every git operation below runs there, exactly
/// where an ordinary `git push` from that command would have run.
pub(super) fn execute_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let (url, refspec) = plan_governed_push(source, cwd, caveats, gate)?;
    let output = crate::git_hardening::credentialed_git(cwd, &["push", &url, &refspec])
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    Ok(render_git_process_output(
        &format!("git push {url} {refspec}"),
        &output,
    ))
}

/// Everything [`execute_governed_push`] must verify BEFORE it may spawn a
/// process at all: argv shape, A1's hostile-config refusal, main/master
/// refusal, A2's URL resolution/scheme validation, and the net-gate prompt.
/// Split out from [`execute_governed_push`] so the test suite can pin every
/// refusal and the exact `(url, refspec)` a granted happy path resolves to
/// without needing a real reachable forge — the same boundary amendment A4
/// already draws for `gh pr create`'s live-forge exercise.
fn plan_governed_push(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<(String, String), String> {
    let argv = standalone_literal_argv(source)?;
    let request = parse_governed_push(&argv)?;

    // Finding 6 (issue-1188 review #2641): this broker runs entirely
    // outside the confined shell's filesystem/exec fence, so it must bind
    // itself to the SAME authority a confined command would have needed —
    // read access to the repository it is about to operate on, and exec
    // authority for `git` itself. Without this, a model-chosen `cwd` naming
    // some OTHER operator repository (one the confined fence would have
    // refused to even read) reaches a real host-side git process.
    let cwd_str = cwd.to_string_lossy();
    if !caveats.permits_fs_read(&cwd_str) {
        return Err(format!(
            "refused: '{cwd_str}' is outside this session's filesystem read authority"
        ));
    }
    if !caveats.permits_exec("git") {
        return Err("refused: no exec authority for 'git'".to_string());
    }

    // A1: repo-local config that could redirect the push's transport,
    // credentials, or hooks is hostile input, refused before anything spawns.
    let listing = crate::git_hardening::local_config_listing(cwd).map_err(|e| e.to_string())?;
    if let Some(key) = crate::git_hardening::hostile_push_config_key(&listing) {
        return Err(format!(
            "refused: repo-local git config sets '{key}', which a governed push \
             cannot safely honor (transport/credential/hook gadget) — remove it \
             from this repository's .git/config and retry"
        ));
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

    // A2: push the resolved URL, not the bare remote name — a repo-local
    // `insteadOf` rewrite is already refused above. Resolve it through
    // `credentialed_git` (the SAME config view — including the operator's
    // own ambient global config — the actual push below runs under), not
    // `hardened_git`'s ambient-config-free view: otherwise a global
    // `url.*.insteadOf` could rewrite the destination AFTER the host check
    // passes here but BEFORE the credentialed push dials it.
    let url = resolve_remote_url_for_push(cwd, &request.remote)?;
    let host = crate::git_hardening::push_url_host(&url)?;

    ensure_net_granted(
        caveats,
        gate,
        &host,
        &format!("push branch '{branch}' to {host} ({url})"),
    )?;

    // Finding 2: full ref identity, never a bare short name that could
    // collide with `refs/heads/refs/heads/main`-shaped input, and no leading
    // character `git push` would ever read as the force-push prefix.
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    Ok((url, refspec))
}

/// [`crate::git_hardening::resolve_remote_url`], but through
/// [`crate::git_hardening::credentialed_git`]'s config view (finding 1,
/// issue-1188 review #2641) — the same process family that will actually
/// dial the URL, so an operator-global `url.*.insteadOf` rewrite is captured
/// by the SAME host check that authorizes it, rather than checked against
/// `hardened_git`'s ambient-config-free resolution and then silently
/// rewritten by the time the credentialed push runs.
fn resolve_remote_url_for_push(cwd: &Path, remote: &str) -> Result<String, String> {
    let output = crate::git_hardening::credentialed_git(cwd, &["remote", "get-url", remote])
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "no such remote '{remote}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

/// issue-1188 amendment A4: execute a governed `gh pr create`, host-side, for
/// the same reason as [`execute_governed_push`] (the confined child cannot
/// reach the forge at all).
pub(super) fn execute_governed_pr_create(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<String, String> {
    let plan = plan_governed_pr_create(source, cwd, caveats, gate)?;
    let output = crate::git_hardening::credentialed_gh(
        cwd,
        &[
            "pr",
            "create",
            "--repo",
            &plan.repo,
            "--base",
            &plan.base,
            "--head",
            &plan.head,
            "--title",
            &plan.title,
            "--body",
            &plan.body,
        ],
    )
    .map_err(|e| e.to_string())?
    .output()
    .map_err(|e| e.to_string())?;
    Ok(render_git_process_output(
        &format!(
            "gh pr create --repo {} --base {} --head {} ...",
            plan.repo, plan.base, plan.head
        ),
        &output,
    ))
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
    let argv = standalone_literal_argv(source)?;
    let request = parse_governed_pr_create(&argv)?;

    // Finding 6: same cwd authority/exec binding as the push broker.
    let cwd_str = cwd.to_string_lossy();
    if !caveats.permits_fs_read(&cwd_str) {
        return Err(format!(
            "refused: '{cwd_str}' is outside this session's filesystem read authority"
        ));
    }
    if !caveats.permits_exec("gh") {
        return Err("refused: no exec authority for 'gh'".to_string());
    }

    // Finding 3: A1's hostile-config refusal never ran on this path at all —
    // `gh` shells out to Git for repository/branch resolution too, so the
    // same transport/credential/hook gadgets [`hostile_push_config_key`]
    // screens for a push apply here.
    let listing = crate::git_hardening::local_config_listing(cwd).map_err(|e| e.to_string())?;
    if let Some(key) = crate::git_hardening::hostile_push_config_key(&listing) {
        return Err(format!(
            "refused: repo-local git config sets '{key}', which a governed PR create \
             cannot safely honor (transport/credential/hook gadget) — remove it \
             from this repository's .git/config and retry"
        ));
    }

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

/// Render a completed host-side git/gh process's output the same way a
/// confined-shell command's output would read to the model: the argv that
/// ran, then stdout/stderr, then an explicit exit-code note on failure (a
/// non-zero exit is real command output, not a broker refusal).
fn render_git_process_output(rendered_argv: &str, output: &std::process::Output) -> String {
    let mut text = format!("$ {rendered_argv}\n");
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        text.push_str(&format!(
            "\n(exited {})",
            output
                .status
                .code()
                .map_or_else(|| "without a status code".to_string(), |c| c.to_string())
        ));
    }
    text
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
            &Caveats::top(),
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
                plan_governed_push(source, repo.path(), &Caveats::top(), &mut None).unwrap_err();
            assert!(err.contains("no flags"), "{source}: {err}");
        }
    }

    /// Would have failed before A1: a repo-local `core.sshCommand` gadget
    /// would otherwise run silently on the FIRST git invocation planning
    /// performs (`config --local --list` itself); this pins that the plan
    /// refuses before any push-shaped command reaches the network stage.
    #[test]
    fn hostile_repo_local_config_is_refused_and_never_reached() {
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
        let err = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &Caveats::top(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("core.sshcommand"), "{err}");
        assert!(
            !sentinel.exists(),
            "the hostile sshCommand payload must never run"
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
                &Caveats::top(),
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
        let (url, refspec) = plan_governed_push(
            "git push origin task:task",
            repo.path(),
            &Caveats::top(),
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(refspec, "refs/heads/task:refs/heads/task");

        // Bare `git push` (no operands) resolves the same way, from origin +
        // the repository's own branch.
        let (url2, refspec2) =
            plan_governed_push("git push", repo.path(), &Caveats::top(), &mut None).unwrap();
        assert_eq!(url2, url);
        assert_eq!(refspec2, refspec);
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
            &Caveats::top(),
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
            &Caveats::top(),
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
            &Caveats::top(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("github.com"), "{err}");
    }

    /// Finding 3 (issue-1188 review #2641): the PR-create broker never ran
    /// A1's hostile-config check at all. Would have failed before the fix:
    /// `plan_governed_pr_create` resolved straight through to `Ok`.
    #[test]
    fn pr_create_refuses_hostile_repo_local_config() {
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
        let err = plan_governed_pr_create(
            "gh pr create --title t --body b",
            repo.path(),
            &Caveats::top(),
            &mut None,
        )
        .unwrap_err();
        assert!(err.contains("core.sshcommand"), "{err}");
    }

    /// Finding 6 (issue-1188 review #2641): the PR-create broker must bind
    /// itself to the same authority the push broker does.
    #[test]
    fn pr_create_requires_exec_and_read_authority() {
        let repo = repo_on_feature_branch();
        let no_exec = Caveats {
            exec: crate::caveats::Scope::only([]),
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
