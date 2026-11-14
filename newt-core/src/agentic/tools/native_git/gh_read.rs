//! Host-side gh reads. Shares PR-create's source-bound parser, gate, trusted
//! executables and private staging directory; never executes model shell text.
use super::*;
use crate::git_staging as staging;

pub(in crate::agentic::tools) struct ReadOutcome {
    pub stdout: String,
    pub code: Option<i32>,
}

fn read_verb(kind: &str, verb: &str) -> bool {
    matches!(
        (kind, verb),
        ("issue" | "pr", "view" | "list" | "status") | ("pr", "diff" | "checks")
    )
}

/// The same loose trigger / strict executor split as PR-create: unsupported
/// wrappers around a recognized read are refused, never silently stripped.
pub(in crate::agentic::tools) fn recognizes(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_gh)
                && command
                    .argv
                    .get(1)
                    .and_then(|a| literal(a))
                    .zip(command.argv.get(2).and_then(|a| literal(a)))
                    .is_some_and(|(kind, verb)| read_verb(&kind, &verb))
        }) || inspection
            .constructs
            .iter()
            .any(|c| c.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

// A repo spec cannot carry a URL, port, query, option or gh branch/URL selector.
fn repository(spec: &str) -> Result<(String, String), &'static str> {
    let parts: Vec<_> = spec.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => ("github.com", *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return Err("expected [host/]owner/repo"),
    };
    let host = crate::caveats::parse_net_host(host).map_err(|_| "invalid forge host")?;
    // go-gh NormalizeHostname collapses GitHub subdomains and nested tenancy
    // names. Refuse aliases so approval and the argv name the SAME authority.
    // https://github.com/cli/go-gh/blob/trunk/pkg/auth/auth.go
    if host.ends_with(".github.com")
        || host.ends_with(".github.localhost")
        || host
            .strip_suffix(".ghe.com")
            .is_some_and(|tenant| tenant.contains('.'))
    {
        return Err("use the canonical forge hostname, not an API or subdomain alias");
    }
    let valid = |v: &str| {
        !v.is_empty()
            && !v.starts_with(['-', '.'])
            && v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    };
    if !valid(owner) || !valid(name) {
        return Err("invalid repository owner or name");
    }
    Ok((host.clone(), format!("{host}/{owner}/{name}")))
}

/// Rebuild a small allowlist of data-only flags. No browser, pager, template,
/// jq, watch loop, URL selector, file input/output, extension or alias surface.
fn arguments(argv: &[String]) -> Result<(Vec<String>, Option<String>), &'static str> {
    if argv.len() < 3 || !is_gh(&argv[0]) || !read_verb(&argv[1], &argv[2]) {
        return Err("unsupported gh read command");
    }
    let (kind, verb) = (argv[1].as_str(), argv[2].as_str());
    let mut result = vec![kind.into(), verb.into()];
    let (mut repo, mut number) = (None, false);
    let mut i = 3;
    while i < argv.len() {
        let word = &argv[i];
        let (flag, inline) = word
            .split_once('=')
            .map_or((word.as_str(), None), |(f, v)| (f, Some(v)));
        let canonical = match (verb, flag) {
            (_, "--repo" | "-R") => "--repo",
            (_, "--json") if verb != "diff" => "--json",
            ("list", "--limit" | "-L") => "--limit",
            ("list", "--state" | "-s") => "--state",
            ("view", "--comments" | "-c") => "--comments",
            ("diff", "--name-only") => "--name-only",
            ("diff", "--patch") => "--patch",
            ("checks", "--required") => "--required",
            ("status", "--conflict-status" | "-c") if kind == "pr" => "--conflict-status",
            _ if !word.starts_with('-')
                && inline.is_none()
                && matches!(verb, "view" | "diff" | "checks")
                && !number
                && word.parse::<u64>().is_ok_and(|n| n > 0) =>
            {
                number = true;
                result.push(word.clone());
                i += 1;
                continue;
            }
            _ => return Err("unsupported gh read argument"),
        };
        if matches!(canonical, "--repo" | "--json" | "--limit" | "--state") {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    argv.get(i)
                        .map(String::as_str)
                        .ok_or("missing gh read flag value")?
                }
            };
            if value.is_empty() || value.starts_with('-') {
                return Err("invalid gh read flag value");
            }
            match canonical {
                "--repo" => {
                    if repo.is_some() {
                        return Err("duplicate repository flag");
                    }
                    repository(value)?;
                    repo = Some(value.into());
                }
                "--limit" if !value.parse::<usize>().is_ok_and(|n| (1..=100).contains(&n)) => {
                    return Err("limit must be 1 through 100")
                }
                "--state"
                    if !matches!(value, "open" | "closed" | "all")
                        && !(kind == "pr" && value == "merged") =>
                {
                    return Err("invalid state")
                }
                "--json"
                    if !value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b',') =>
                {
                    return Err("invalid JSON fields")
                }
                _ => {}
            }
            if canonical != "--repo" {
                result.extend([canonical.into(), value.into()]);
            }
        } else {
            if inline.is_some() {
                return Err("unexpected gh read flag value");
            }
            result.push(canonical.into());
        }
        i += 1;
    }
    if matches!(verb, "view" | "diff" | "checks") && !number {
        return Err("a positive issue or pull request number is required");
    }
    Ok((result, repo))
}

fn origin_repository(cwd: &Path, caveats: &Caveats) -> Result<String, staging::Refusal> {
    let held = staging::HeldRoots::bind(&caveats.fs_read)?;
    let (common, _) = staging::discover_git_dirs(cwd, &held)?;
    let url = staging::literal_remote_url(&common, "origin", &held)?;
    let (host, owner, name) = crate::git_hardening::forge_owner_repo(&url)
        .ok_or_else(|| "invalid remote repository".to_string())?;
    Ok(format!("{host}/{owner}/{name}"))
}

pub(in crate::agentic::tools) fn execute(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    #[cfg(test)] probe: Option<&super::super::gh_read_tests::Probe>,
) -> String {
    let command = match push_command::GovernedCommand::parse(source) {
        Ok(command) => command,
        Err(reason) => return format!("refused: {reason}"),
    };
    let mut run = || -> Result<ReadOutcome, String> {
        let (mut argv, repo) = arguments(&command.argv).map_err(str::to_owned)?;
        if !caveats.permits_exec("gh")
            || !crate::caveats::permits_path(&caveats.fs_read, &cwd.to_string_lossy())
        {
            return Err("gh execution and workspace read authority are required".into());
        }
        let resolve = || {
            origin_repository(cwd, caveats)
                .map_err(|_| "cannot resolve an authorized origin repository".to_string())
        };
        #[cfg(test)]
        let resolve = || probe.map_or_else(resolve, |p| Ok(p.remote.clone()));
        let repo = repo.map_or_else(resolve, Ok)?;
        let (host, repo) = repository(&repo).map_err(str::to_owned)?;
        let mut approved = vec![host.clone()];
        ensure_net_granted(caveats, gate, &host, "read forge metadata")?;
        if cloud_host(&host) {
            let api = format!("api.{host}");
            ensure_net_granted(caveats, gate, &api, "read forge metadata")?;
            approved.push(api);
        }
        argv.extend(["--repo".into(), repo]);
        // FIRST executable discovery/spawn is after every network decision.
        #[cfg(test)]
        if let Some(probe) = probe {
            return probe.run(&host, &argv);
        }
        run_staged_read(&argv, &host, &approved, caveats)
    };
    match run() {
        Ok(outcome) => {
            let stdout = if outcome.stdout.is_empty() && outcome.code != Some(0) {
                "refused: gh read returned no output".into()
            } else {
                outcome.stdout
            };
            let mut rendered = super::super::output_budget::cap_model_output(
                &command.render_status(
                    stdout.trim_end_matches('\n').into(),
                    outcome.code.unwrap_or(1),
                ),
                4064,
            );
            match outcome.code {
                Some(0) => {}
                Some(code) => rendered.push_str(&format!("\n(exit {code})")),
                None => rendered.push_str("\n(terminated without an exit code)"),
            }
            rendered
        }
        Err(reason) => super::super::output_budget::cap_model_output(
            &command.render(
                format!(
                    "refused: {}",
                    reason.strip_prefix("refused: ").unwrap_or(&reason)
                ),
                false,
            ),
            4096,
        ),
    }
}

#[cfg(unix)]
fn run_staged_read(
    argv: &[String],
    host: &str,
    approved: &[String],
    caveats: &Caveats,
) -> Result<ReadOutcome, String> {
    use std::os::unix::fs::PermissionsExt;
    let prepare = || -> Result<_, staging::Refusal> {
        staging::preflight_availability(caveats).map_err(|e| e.to_string())?;
        let tools =
            staging::TrustedTools::authenticate(staging::TrustContext::bind(&caveats.fs_write)?)?;
        // A short owner-only parent keeps the socket below macOS's 104-byte limit.
        // StagingRepo still authenticates it against the bound model-write roots.
        let short = tempfile::Builder::new()
            .prefix("ngh-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .map_err(|e| e.to_string())?;
        let staging = staging::StagingRepo::create(&short.path().join("s"), tools.ctx())?;
        let mut child = tools.gh(argv)?;
        pin_environment(&mut child, staging.path(), host);
        if let Some(dir) = staging::trusted_gh_config_dir(tools.ctx())? {
            let held = staging::HeldRoots::bind(&caveats.fs_read)?;
            let file = dir.join("hosts.yml");
            if file.exists() {
                staging::trust_check(&file, tools.ctx())?;
                let text = held
                    .read_to_string(&dir, Path::new("hosts.yml"))?
                    .unwrap_or_default();
                let config: serde_yaml::Value = serde_yaml::from_str(&text)
                    .map_err(|_| "invalid gh credentials".to_string())?;
                // Copy only the selected host's token, never configuration/aliases.
                if let Some(token) = config
                    .get(host)
                    .and_then(|h| h.get("oauth_token"))
                    .and_then(serde_yaml::Value::as_str)
                {
                    child.env(
                        if cloud_host(host) {
                            "GH_TOKEN"
                        } else {
                            "GH_ENTERPRISE_TOKEN"
                        },
                        token,
                    );
                }
            }
        }
        Ok((short, staging, child))
    };
    let (_short, staging, mut child) = prepare().map_err(|_| {
        "host gh read unavailable; check executable trust and read grants".to_string()
    })?;
    super::gh_http::with_approved_http(staging.path(), approved, || {
        let output = child
            .output()
            .map_err(|_| "could not start host gh read".to_string())?;
        Ok(ReadOutcome {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            code: output.status.code(),
        })
    })
}

#[cfg(not(unix))]
fn run_staged_read(
    _: &[String],
    _: &str,
    _: &[String],
    _: &Caveats,
) -> Result<ReadOutcome, String> {
    Err("host gh reads require the Unix socket request boundary on this platform".into())
}

// gh's public/cloud instances have an api.<host> endpoint; GHES uses the
// same host. https://github.com/cli/cli/blob/trunk/internal/ghinstance/host.go
fn cloud_host(host: &str) -> bool {
    host == "github.com" || host == "github.localhost" || host.ends_with(".ghe.com")
}

// Pure command construction; TrustedTools already supplies env_clear and the
// authenticated PATH. Never forward ambient tokens or repository selectors.
#[cfg(any(unix, test))]
fn pin_environment(child: &mut std::process::Command, dir: &Path, host: &str) {
    child
        .current_dir(dir)
        .env("HOME", dir)
        .env("GH_CONFIG_DIR", dir)
        .env("GH_HOST", host)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_TELEMETRY", "0")
        .env("DO_NOT_TRACK", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat");
    for key in [
        "GH_REPO",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "SSH_AUTH_SOCK",
    ] {
        child.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gh_read_child_pinning_overrides_ambient_selectors_and_credentials() {
        // Command construction is pure: this executable is never resolved or run.
        let mut child = std::process::Command::new("never-executed");
        for key in [
            "GH_HOST",
            "GH_CONFIG_DIR",
            "HOME",
            "GH_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_REPO",
        ] {
            child.env(key, "untrusted");
        }
        pin_environment(&mut child, Path::new("/private-staging"), "github.com");
        let env: std::collections::BTreeMap<_, _> = child.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("GH_HOST")],
            Some(std::ffi::OsStr::new("github.com"))
        );
        for key in ["HOME", "GH_CONFIG_DIR"] {
            assert_eq!(
                env[std::ffi::OsStr::new(key)],
                Some(std::ffi::OsStr::new("/private-staging"))
            );
        }
        for key in ["GH_TOKEN", "GH_ENTERPRISE_TOKEN", "GH_REPO"] {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
    }
}
