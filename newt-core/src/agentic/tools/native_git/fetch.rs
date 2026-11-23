//! Host-side broker for `git fetch` (the read-side sibling of the governed
//! push in `native_git.rs`, built from the same `crate::git_staging` phases).
//!
//! A confined child cannot dial a remote: a host-scoped `net` grant is
//! narrowed to no network for spawned children, and the sandbox git carries
//! `core.sshCommand=false` so the operator's SSH keys stay out of it. So the
//! fetch is split in three, and only the middle step touches the network:
//!
//! 1. **Seed** — a confined git copies the workspace's existing
//!    remote-tracking tips into a fresh staging repo, so the dial downloads
//!    only what is new.
//! 2. **Dial** — the trusted host git fetches the remote's branches and tags
//!    into staging, with the operator's credentials or SSH agent. It never
//!    has the workspace's `.git/config` in its config chain.
//! 3. **Deliver** — a confined git, with no network authority, fetches from
//!    staging into the workspace's `refs/remotes/<remote>/*`.
//!
//! Both copy steps require the executor's deny-all network floor. Platforms
//! that cannot establish a kernel witness refuse the seed before dialing.
//! macOS admits audited, unprivileged Seatbelt launches with that witness. Failed delivery can leave refs, objects or FETCH_HEAD changed;
//! the broker reports that uncertainty rather than promising a rollback.
//!
//! **It gets out of the way.** [`wanted`] is false under unrestricted `net`
//! (ambient mode), where the shell's own git can dial. [`execute`] returns
//! `None`, and the command runs in the shell exactly as before, for any form
//! or remote it does not handle: a flag it does not model, a local-path
//! remote, a session that cannot write the repository.

use super::super::governed_command::GovernedCommand;
use super::{ensure_net_granted, invocation, is_git, PermissionGate};
use crate::git_staging as staging;
use agent_mesh_protocol::caveats::Scope;
use newt_policy::caveats::{permits_path, Caveats, CaveatsExt};
use std::path::Path;

/// What the model's `git fetch` asked for. Only these shapes are brokered.
#[derive(Debug, PartialEq, Eq)]
struct Fetch {
    remote: String,
    /// One branch, or every branch when `None`.
    branch: Option<String>,
    prune: bool,
}

/// What the broker did with a `git fetch`.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::agentic::tools) enum Brokered {
    /// The broker ran or refused it; this is the result to show the model.
    Answered(String),
    /// The remote is a path on this machine: no network is involved, so the
    /// caller runs the command in the fence with no network prompt (#3148).
    LocalRemote,
}

impl Brokered {
    #[cfg(all(test, unix))]
    pub(in crate::agentic::tools) fn answer(self) -> String {
        match self {
            Self::Answered(result) => result,
            Self::LocalRemote => panic!("a local remote has no brokered answer"),
        }
    }
}

/// Whether a remote's literal URL names a path on this machine (absolute,
/// relative, `file://` or a Windows drive), not a network host. A URL with a
/// scheme (`https://`, `ssh://`) or an scp-like `host:path` is not local, and
/// neither is a `transport::address` helper (`ext::`): those keep the prompt.
fn is_local_remote(url: &str) -> bool {
    // Empty, or shaped like an option: never a path to read.
    if url.is_empty() || url.starts_with('-') {
        return false;
    }
    if url.starts_with("file://") {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    match url.find([':', '/', '\\']) {
        // `host:path` and `transport::address` have a colon before any slash.
        Some(i) if url.as_bytes()[i] == b':' => {
            // A drive letter is a path only on Windows; elsewhere git reads
            // `x:/y` as ssh to a host named `x`.
            cfg!(windows)
                && i == 1
                && url.as_bytes()[0].is_ascii_alphabetic()
                && url[2..].starts_with(['/', '\\'])
        }
        _ => true,
    }
}

fn parse(argv: &[String]) -> Option<Fetch> {
    let (verb, args, same_repository) = invocation(argv).ok()?;
    if !argv.first().is_some_and(|a| is_git(a)) || verb != "fetch" || !same_repository {
        return None;
    }
    let mut fetch = Fetch {
        remote: "origin".to_string(),
        branch: None,
        prune: false,
    };
    let mut operands = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-p" | "--prune" => fetch.prune = true,
            "-q" | "--quiet" | "-v" | "--verbose" => {}
            flag if flag.starts_with('-') => return None,
            operand => operands.push(operand),
        }
    }
    match operands[..] {
        [] => {}
        [remote] => fetch.remote = remote.to_string(),
        [remote, branch] if staging::validate_branch_name(branch).is_ok() => {
            fetch.remote = remote.to_string();
            fetch.branch = Some(branch.to_string());
        }
        _ => return None,
    }
    // A remote NAME, never a URL or path: the destination is read from config.
    fetch
        .remote
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .then_some(fetch)
}

/// Whether `source` is a `git fetch` this broker should be offered. False
/// under unrestricted `net`: there the shell's own git can dial.
pub(in crate::agentic::tools) fn wanted(source: &str, caveats: &Caveats) -> bool {
    !matches!(caveats.net, Scope::All)
        && GovernedCommand::parse(source).is_ok_and(|command| parse(&command.argv).is_some())
}

/// Run a brokered fetch. `None` means "not handled here": no transfer or
/// approval occurred, and the caller uses the consent-first fallback;
/// [`Brokered::LocalRemote`] means the same without the prompt.
pub(in crate::agentic::tools) fn execute(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Option<Brokered> {
    execute_with(source, cwd, caveats, gate, |url| {
        crate::git_hardening::push_url_host(url).ok()
    })
}

/// [`execute`] with the URL-to-host rule injected: the test seam that lets a
/// real run dial a local repository instead of a forge.
pub(super) fn execute_with(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    dialable_host: impl FnOnce(&str) -> Option<String>,
) -> Option<Brokered> {
    let command = GovernedCommand::parse(source).ok()?;
    let fetch = parse(&command.argv)?;
    super::broker_available(caveats).ok()?;
    if !permits_path(&caveats.fs_read, &cwd.to_string_lossy()) || !caveats.permits_exec("git") {
        return None;
    }
    let held = staging::HeldRoots::bind(&caveats.fs_read).ok()?;
    let (common_dir, git_dir) = staging::discover_git_dirs(cwd, &held).ok()?;
    // The LITERAL url, as for a push: what the operator approves is dialed.
    let url = staging::literal_remote_url(&common_dir, &fetch.remote, &held).ok()?;
    let Some(host) = dialable_host(&url) else {
        // A path on this machine needs no network: the shell's own git reads
        // it in the fence, as before #3097's prompt, instead of asking for
        // egress the command does not use (#3148). Decided before the gates
        // below, which only a dial needs: an adopted worktree cannot write the
        // shared git directory, yet still has nothing to ask for.
        return is_local_remote(&url).then_some(Brokered::LocalRemote);
    };
    let tools =
        staging::TrustedTools::authenticate(staging::TrustContext::bind(&caveats.fs_write).ok()?)
            .map_err(|refusal| unavailable_notice(&refusal, std::io::stdout()))
            .ok()?;
    let writable = [&common_dir, &git_dir].map(|dir| dir.to_string_lossy().into_owned());
    if !writable
        .iter()
        .all(|dir| permits_path(&caveats.fs_write, dir))
    {
        return None;
    }

    // From here the broker owns the command: it answers, it never falls back.
    let result = ensure_net_granted(caveats, gate, &host, &format!("fetch from {host} ({url})"))
        .map_err(|_| {
            format!(
                "denied: the operator did not grant network access to '{host}' for this \
                 `git fetch`. No command ran. Do not retry it; take a different approach."
            )
        })
        .and_then(|()| {
            let plan = Plan {
                fetch: &fetch,
                url: &url,
                host: &host,
                cwd,
                writable: &writable,
                tools: &tools,
            };
            run(&plan, caveats, &held)
        });
    let success = result.is_ok();
    Some(Brokered::Answered(
        command.render(result.unwrap_or_else(|reason| reason), success),
    ))
}

/// Tells the operator why a fetch was not brokered. The model sees nothing:
/// the command falls through to the shell and its one-command egress prompt.
fn unavailable_notice(refusal: &staging::Refusal, mut notice: impl std::io::Write) {
    crate::agentic::display::write_harness_notice(
        &mut notice,
        "the git fetch broker is unavailable (a host tool failed its trust check); \
         falling back to a one-command network prompt. SSH remotes will not fetch.",
        false,
    );
    super::notice_trust_hints(refusal, notice);
}

struct Plan<'a> {
    fetch: &'a Fetch,
    url: &'a str,
    host: &'a str,
    cwd: &'a Path,
    /// The repository's common and per-worktree git directories: all the
    /// deliver step may write.
    writable: &'a [String; 2],
    tools: &'a staging::TrustedTools,
}

/// Seed, dial, deliver. `Ok` names the remote and carries the deliver step's
/// own summary of the refs it changed, or says nothing changed. Every
/// `Err` is harness-authored: the dial's raw output can quote a credential
/// helper and is dropped.
fn run(plan: &Plan, caveats: &Caveats, held: &staging::HeldRoots) -> Result<String, String> {
    let failed = |step: &str| {
        format!(
            "failed: `git fetch` could not {step}. Nothing in the repository changed. Do not \
             retry it unchanged."
        )
    };
    let state_dir = crate::config::Config::user_config_dir()
        .ok_or_else(|| failed("find a state directory for its staging repository"))?
        .join("staging");
    let repo = staging::StagingRepo::create(&state_dir, plan.tools.ctx())
        .map_err(|_| failed("create its staging repository"))?;
    staging::import_credentials(&repo, plan.tools)
        .map_err(|_| failed("import the operator's git credential configuration"))?;
    let staging_dir = repo.path().to_string_lossy().into_owned();
    let remote = &plan.fetch.remote;

    let seed = format!("+refs/remotes/{remote}/*:refs/seed/*");
    let workspace = plan.cwd.to_string_lossy();
    let seeded = staging::confined::confined_git(
        plan.tools,
        &repo,
        &[
            "-C",
            &staging_dir,
            "fetch",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-tags",
            "--no-write-fetch-head",
            &workspace,
            &seed,
        ],
        Scope::only([staging_dir.clone()]),
        caveats,
        held,
    );
    if !seeded.is_ok_and(|out| out.success) {
        return Err(failed("copy the repository's known history into staging"));
    }
    staging::remove_alternates(&repo).map_err(|_| failed("isolate its staging repository"))?;

    let heads = match &plan.fetch.branch {
        Some(branch) => format!("+refs/heads/{branch}:refs/heads/{branch}"),
        None => "+refs/heads/*:refs/heads/*".to_string(),
    };
    let dialed = plan
        .tools
        .git([
            "-c",
            "http.followRedirects=false",
            "fetch",
            "--no-write-fetch-head",
            "--no-tags",
            plan.url,
            &heads,
            "+refs/tags/*:refs/tags/*",
        ])
        .current_dir(repo.path())
        .output();
    if !dialed.is_ok_and(|out| out.status.success()) {
        return Err(format!(
            "failed: `git fetch` could not download from {} ({}): the remote refused, the \
             operator's credentials were not accepted, or the network is unreachable. Nothing \
             in the repository changed. Do not retry it unchanged; ask the operator.",
            plan.host, plan.url
        ));
    }

    let tracking = match &plan.fetch.branch {
        Some(branch) => format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}"),
        None => format!("+refs/heads/*:refs/remotes/{remote}/*"),
    };
    let mut deliver = vec![
        "-c",
        "gc.auto=0",
        "-c",
        "core.hooksPath=/dev/null",
        "-C",
        &workspace,
        "fetch",
        "--no-recurse-submodules",
        "--no-auto-maintenance",
    ];
    if plan.fetch.prune {
        deliver.push("--prune");
    }
    deliver.extend([staging_dir.as_str(), tracking.as_str()]);
    delivery_result(
        staging::confined::confined_git(
            plan.tools,
            &repo,
            &deliver,
            Scope::only(plan.writable.iter().cloned()),
            caveats,
            held,
        ),
        &staging_dir,
        plan.url,
    )
}

fn delivery_result(
    result: Result<crate::confined_exec::ConfinedOutput, staging::Refusal>,
    staging_dir: &str,
    url: &str,
) -> Result<String, String> {
    let delivered = result.ok()
    .filter(|out| out.success)
    .ok_or_else(|| "failed: `git fetch` could not complete delivery. Repository refs, objects or FETCH_HEAD may have changed. Inspect the repository before retrying.".to_string())?;
    // Git names the source it fetched from; that is staging, so say the remote.
    let summary = String::from_utf8_lossy(&delivered.stderr).replace(staging_dir, url);
    // #3109: git prints nothing when nothing changed, and an empty result
    // reads as a failure. Say what happened either way.
    Ok(match summary.trim_end() {
        "" => format!("Fetched from {url}: already up to date."),
        changed => format!("Fetched from {url}.\n{changed}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed ref lock does not undo another ref's successful update. The
    /// ignored real Git test supplies this mixed outcome from actual refs.
    #[test]
    fn failed_delivery_does_not_claim_the_repository_is_unchanged() {
        let output = crate::confined_exec::ConfinedOutput {
            success: false,
            code: Some(1),
            stdout: Vec::new(),
            stderr:
                b"* [new branch] good -> origin/good\n! [rejected] locked -> origin/locked\nPRIVATE"
                    .to_vec(),
            sandbox_kind: crate::sandbox_seam::SandboxKind::Landlock,
            timed_out: false,
        };
        for result in [
            Ok(output),
            Err(staging::Refusal::from("PRIVATE".to_string())),
        ] {
            let error =
                delivery_result(result, "/staging", "https://example.invalid/repo").unwrap_err();
            assert!(
                !error.contains("Nothing in the repository changed"),
                "delivery failure falsely claimed no changes: {error}"
            );
            assert!(error.contains("may have changed"), "{error}");
            assert!(
                !error.contains("PRIVATE"),
                "raw failure output leaked: {error}"
            );
        }
    }

    /// #3109: a delivery that changed nothing printed nothing, and the model
    /// read the empty result as a failure. Either way the result says what
    /// was fetched from, and never names the staging directory.
    #[test]
    fn a_delivered_fetch_says_what_happened_even_when_nothing_changed() {
        let delivered = |stderr: &str| {
            delivery_result(
                Ok(crate::confined_exec::ConfinedOutput {
                    success: true,
                    code: Some(0),
                    stdout: Vec::new(),
                    stderr: stderr.as_bytes().to_vec(),
                    sandbox_kind: crate::sandbox_seam::SandboxKind::Landlock,
                    timed_out: false,
                }),
                "/staging",
                "https://example.invalid/repo",
            )
            .unwrap()
        };
        assert_eq!(
            delivered(""),
            "Fetched from https://example.invalid/repo: already up to date."
        );
        assert_eq!(
            delivered("From /staging\n * [new branch] a -> origin/a\n"),
            "Fetched from https://example.invalid/repo.\nFrom https://example.invalid/repo\n \
             * [new branch] a -> origin/a"
        );
    }

    fn argv(command: &str) -> Vec<String> {
        command.split(' ').map(str::to_string).collect()
    }

    /// A fall-through for an untrusted host tool is announced to the operator,
    /// and the refusal's detail (which can quote repository bytes) is not.
    #[test]
    fn an_unavailable_broker_tells_the_operator_it_fell_through() {
        let mut sink = Vec::new();
        unavailable_notice(&staging::Refusal::from("DETAIL".to_string()), &mut sink);
        let notice = String::from_utf8(sink).unwrap();
        assert!(
            notice.contains("git fetch broker is unavailable"),
            "{notice}"
        );
        assert!(!notice.contains("DETAIL"), "{notice}");
    }

    /// #3148: which literal remote URLs are paths on this machine. Anything with
    /// a scheme, an scp-like `host:path` or a `transport::address` helper stays
    /// a network remote, so it keeps the prompt.
    #[test]
    fn only_a_path_on_this_machine_is_a_local_remote() {
        for local in [
            "/srv/git/repo.git",
            "../sibling",
            "./repo",
            "repo",
            "file:///srv/git/repo.git",
        ] {
            assert!(is_local_remote(local), "{local} is a path");
        }
        // A drive letter is a path on Windows only; on Unix git dials host `C`.
        for drive in ["C:\\work\\repo", "C:/work/repo", "x:/y"] {
            assert_eq!(is_local_remote(drive), cfg!(windows), "{drive}");
        }
        for network in [
            "https://forge.example/o/r.git",
            "ssh://git@forge.example/o/r.git",
            "git@forge.example:o/r.git",
            "forge.example:o/r.git",
            "ext::sh -c evil",
            "fd::3",
            "git://forge.example/o/r.git",
            "a:b/c",
            "-oProxyCommand=evil",
            "",
        ] {
            assert!(!is_local_remote(network), "{network} is not a path");
        }
    }

    #[test]
    fn the_familiar_fetch_forms_are_brokered() {
        let fetch = |remote: &str, branch: Option<&str>, prune| {
            Some(Fetch {
                remote: remote.to_string(),
                branch: branch.map(str::to_string),
                prune,
            })
        };
        assert_eq!(parse(&argv("git fetch")), fetch("origin", None, false));
        assert_eq!(
            parse(&argv("git fetch --prune")),
            fetch("origin", None, true)
        );
        assert_eq!(
            parse(&argv("git fetch -p upstream")),
            fetch("upstream", None, true)
        );
        assert_eq!(
            parse(&argv("git fetch -q origin main")),
            fetch("origin", Some("main"), false)
        );
    }

    #[test]
    fn anything_else_is_left_to_the_shell() {
        for command in [
            "git pull",
            "git fetch --all",
            "git fetch --depth=1",
            "git fetch origin main:other",
            "git fetch origin a b",
            "git fetch https://example.com/o/r.git",
            "git fetch ../sibling",
            "git -C elsewhere fetch",
            "gh fetch",
        ] {
            assert_eq!(parse(&argv(command)), None, "{command}");
        }
    }

    #[test]
    fn the_broker_is_not_offered_under_unrestricted_net() {
        let scoped = |net| Caveats {
            net,
            ..Caveats::top()
        };
        let confined = scoped(Scope::only(["lms.example.com".to_string()]));
        assert!(wanted("git fetch --prune 2>&1 | head -5", &confined));
        assert!(!wanted("git fetch --prune", &scoped(Scope::All)));
        assert!(!wanted("git status", &confined));
        assert!(!wanted("git fetch && rm -rf x", &confined));
    }
}
