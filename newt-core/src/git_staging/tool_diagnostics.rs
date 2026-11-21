//! Operator preflight for the same tool-path policy enforced by staging.
use super::*;
#[cfg(unix)]
mod repair;

/// Ephemeral diagnostic output; never grants authority to a broker call.
#[derive(Default)]
pub struct ToolTrustReport {
    pub lines: Vec<String>,
    pub hints: Vec<TrustHint>,
    #[cfg(unix)]
    repairs: std::collections::BTreeMap<PathBuf, Result<repair::BoundRepair, String>>,
}

impl ToolTrustReport {
    #[cfg(unix)]
    fn inspect(&mut self, path: &Path, ctx: &TrustContext) {
        match tool_path_trust_hints(path, ctx) {
            Ok(hints) if hints.is_empty() => {
                self.lines.push(format!("{}: trusted path", path.display()));
            }
            Ok(hints) => {
                self.lines
                    .push(format!("{}: writable tool path", path.display()));
                for hint in hints {
                    if !self.hints.contains(&hint) {
                        let bound = repair::BoundRepair::capture(&hint, &repair::Host)
                            .map_err(|e| format!("{e}; re-run `newt doctor`"));
                        if let Err(error) = &bound {
                            self.lines.push(format!(
                                "Automatic repair unavailable for {}: {error}",
                                hint.path.display()
                            ));
                        }
                        self.repairs.insert(hint.path.clone(), bound);
                        self.lines.push(hint.render());
                        self.hints.push(hint);
                    }
                }
            }
            Err(e) => self.lines.push(e.to_string()),
        }
    }

    /// Apply only the displayed write-bit removals, after explicit consent.
    /// An absent operator, blank answer, or generic setup `--yes` is not consent.
    pub fn repair(&self, consent: bool) -> Vec<String> {
        self.repair_with(consent, |hint| {
            #[cfg(unix)]
            {
                let bound = self
                    .repairs
                    .get(&hint.path)
                    .ok_or_else(|| {
                        std::io::Error::other("no diagnosed object; re-run `newt doctor`")
                    })?
                    .as_ref()
                    .map_err(|e| std::io::Error::other(e.clone()))?;
                bound.apply(hint, &repair::Host)
            }
            #[cfg(not(unix))]
            {
                let _ = hint;
                Err(std::io::Error::other(
                    "Unix tool permissions are unsupported on this platform",
                ))
            }
        })
    }

    fn repair_with(
        &self,
        consent: bool,
        mut repair: impl FnMut(&TrustHint) -> std::io::Result<()>,
    ) -> Vec<String> {
        if !consent {
            return vec!["Tool permissions unchanged; run `newt doctor` to re-check.".into()];
        }
        self.hints
            .iter()
            .map(|h| match repair(h) {
                Ok(()) => format!("Repaired tool permissions: {}", h.path.display()),
                Err(e) => format!(
                    "Could not repair {}: {e}; re-run `newt doctor`",
                    h.path.display()
                ),
            })
            .collect()
    }
}

/// Check PATH tools and configured credential helpers without executing any helper.
/// An untrusted git is reported, never executed to discover more tools.
#[must_use]
pub fn diagnose_tool_paths() -> ToolTrustReport {
    let mut report = ToolTrustReport::default();
    #[cfg(not(unix))]
    {
        report
            .lines
            .push("Tool-path Unix permission checks are unsupported on this platform.".into());
        report
    }
    #[cfg(unix)]
    {
        let path = std::env::var_os("PATH");
        let mut ctx = TrustContext::bind(&Scope::none()).expect("empty write scope");
        let mut candidates = BTreeSet::new();
        let git = crate::git_hardening::trusted_git_program(path.as_deref()).ok();
        if let Some(git) = &git {
            if let Ok(resolved) = std::fs::canonicalize(git) {
                ctx = ctx.with_git(&resolved);
            }
        }
        for name in ["git", "gh", "ssh"] {
            match crate::git_hardening::resolve_trusted_program(
                Path::new("."),
                path.as_deref(),
                name,
            ) {
                Ok(tool) => {
                    candidates.insert(tool);
                }
                Err(e) => report.lines.push(format!("{name}: {e}")),
            }
        }
        let dirs: Vec<PathBuf> = path
            .as_deref()
            .into_iter()
            .flat_map(std::env::split_paths)
            .filter(|p| p.is_absolute())
            .collect();
        for dir in &dirs {
            find_helpers(dir, &mut candidates);
        }
        if let Some(git) = git.filter(|git| trust_check(git, &ctx).is_ok()) {
            let run = |args: &[&str]| {
                let mut cmd = Command::new(&git);
                cmd.args(args)
                    .current_dir("/")
                    .env_clear()
                    .envs(governed_child_env(&[], None))
                    .env_remove("GIT_CONFIG_NOSYSTEM")
                    .env_remove("GIT_CONFIG_GLOBAL");
                cmd.output()
            };
            let exec_path = match run(&["--exec-path"]) {
                Ok(out) if out.status.success() => {
                    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
                    ctx = ctx.with_git(&dir);
                    candidates.insert(dir.clone());
                    find_helpers(&dir, &mut candidates);
                    Some(dir)
                }
                _ => {
                    report
                        .lines
                        .push("Could not discover git's credential-helper directory.".into());
                    None
                }
            };
            match run(&[
                "config",
                "--includes",
                "--null",
                "--get-regexp",
                r"^credential\..*helper$",
            ]) {
                Ok(out) if out.status.success() || out.status.code() == Some(1) => {
                    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                        if let Some((_, value)) = std::str::from_utf8(entry)
                            .ok()
                            .and_then(|s| s.split_once('\n'))
                        {
                            helper_candidate(
                                value,
                                exec_path.as_deref(),
                                path.as_deref(),
                                &mut candidates,
                                &mut report.lines,
                            );
                        }
                    }
                }
                _ => report
                    .lines
                    .push("Could not inspect configured credential helpers.".into()),
            }
        } else {
            report.lines.push("Configured helper discovery skipped until git's path is trusted; re-run `newt doctor` after repairs.".into());
        }
        for tool in candidates {
            report.inspect(&tool, &ctx);
        }
        if !report.hints.is_empty() {
            report.lines.push(
                "Homebrew operations may restore group write; run `newt doctor` to re-check."
                    .into(),
            );
        }
        report
    }
}

/// #3152: the same tool-path walk for exec grants that stayed basenames because
/// a directory on their path is writable, with the same consented `chmod g-w`
/// repair `git`, `gh` and `ssh` already get. A name refused for a reason with no
/// writable directory to repair (a tree the session can write, a relative PATH
/// entry, an uninspectable path) is reported as such and offered no repair.
/// Toolchain wrappers (cargo, rustc,
/// ...) are the exception: no path or `chmod` makes them run in a confined
/// `run_command`, so they are sent to `lifecycle action=build` and offered no
/// repair. Names that are simply not installed are not this report's business.
#[must_use]
pub fn diagnose_exec_grants(
    unpinned: &[crate::exec_grants::Unpinned],
    path: Option<&OsStr>,
) -> ToolTrustReport {
    #[cfg(unix)]
    {
        use crate::exec_grants::{is_toolchain_proxy, UnpinnedReason};
        let mut report = ToolTrustReport::default();
        let ctx = TrustContext::bind(&Scope::none()).expect("empty write scope");
        for entry in unpinned
            .iter()
            .filter(|u| {
                matches!(
                    u.reason,
                    UnpinnedReason::Refused(_) | UnpinnedReason::Untrusted
                )
            })
            // Already inspected above, with git's own trust context.
            .filter(|u| !matches!(u.name.as_str(), "git" | "gh" | "ssh"))
        {
            if is_toolchain_proxy(&entry.name) {
                report.lines.push(format!(
                    "{}: a toolchain wrapper; no path or chmod makes it run confined. Run builds and tests with `lifecycle action=build`.",
                    entry.name
                ));
                continue;
            }
            if matches!(entry.reason, UnpinnedReason::Untrusted) {
                // No writable directory for a chmod to repair. The session refused
                // it for a tree it can write, a relative PATH entry, or a path it
                // could not inspect; this walk cannot see the session's write
                // grants, so it would only say "trusted".
                report.lines.push(format!(
                    "{}: not trusted for the session (inside a tree it can write, a relative PATH entry, or a path that could not be inspected); no chmod repair applies.",
                    entry.name
                ));
                continue;
            }
            match crate::git_hardening::resolve_trusted_program(Path::new("."), path, &entry.name) {
                Ok(tool) => report.inspect(&tool, &ctx),
                Err(e) => report.lines.push(format!("{}: {e}", entry.name)),
            }
        }
        report
    }
    #[cfg(not(unix))]
    {
        let _ = (unpinned, path);
        ToolTrustReport::default()
    }
}

#[cfg(unix)]
fn find_helpers(dir: &Path, candidates: &mut BTreeSet<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("git-credential-")
                && entry.path().is_file()
            {
                candidates.insert(entry.path());
            }
        }
    }
}

#[cfg(unix)]
fn helper_candidate(
    value: &str,
    exec_path: Option<&Path>,
    path: Option<&OsStr>,
    candidates: &mut BTreeSet<PathBuf>,
    lines: &mut Vec<String>,
) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    // Never evaluate shell helpers. gh is already inspected independently.
    if let Some(command) = value.strip_prefix('!') {
        if let Some(gh) = command.trim().strip_suffix(" auth git-credential") {
            let gh = Path::new(gh);
            if gh.is_absolute() {
                candidates.insert(gh.into());
                return;
            }
            if gh == Path::new("gh") {
                return;
            }
        }
        lines.push("A shell credential helper cannot be resolved safely; governed staging accepts only the trusted gh auth git-credential form.".into());
        return;
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !value.starts_with(['-', '.'])
    {
        let name = format!("git-credential-{value}");
        if let Some(helper) = exec_path.map(|d| d.join(&name)).filter(|p| p.is_file()) {
            candidates.insert(helper);
        } else if let Ok(helper) =
            crate::git_hardening::resolve_trusted_program(Path::new("."), path, &name)
        {
            candidates.insert(helper);
        } else {
            lines.push(format!("Configured credential helper {name}: not found."));
        }
    } else if Path::new(value).is_absolute() {
        candidates.insert(value.into());
    } else {
        lines.push("A configured credential helper has arguments or unsupported syntax; it was not executed.".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3152: an exec grant refused for a writable directory is walked like `gh`,
    /// with the same repair hint; a toolchain wrapper is sent to the build lane
    /// with no hint; a name that is merely not installed is not reported.
    #[cfg(unix)]
    #[test]
    fn exec_grants_refused_for_a_writable_directory_get_the_repair_hint() {
        use crate::exec_grants::{Unpinned, UnpinnedReason};
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o775)).unwrap();
        for name in ["tool", "cargo"] {
            let program = bin.join(name);
            std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let refused = |name: &str| Unpinned {
            name: name.into(),
            reason: UnpinnedReason::Refused(vec![bin.clone()]),
        };
        let report = diagnose_exec_grants(
            &[
                refused("tool"),
                refused("cargo"),
                Unpinned {
                    name: "absent".into(),
                    reason: UnpinnedReason::NotFound,
                },
                Unpinned {
                    name: "inner".into(),
                    reason: UnpinnedReason::Untrusted,
                },
            ],
            Some(bin.as_os_str()),
        );
        let text = report.lines.join("\n");
        assert!(text.contains("tool: writable tool path"), "{text}");
        assert!(
            report.hints.iter().any(|h| h.path.ends_with("bin")),
            "the writable directory carries the repair hint: {text}"
        );
        assert!(text.contains("cargo: a toolchain wrapper"), "{text}");
        assert!(text.contains("lifecycle action=build"), "{text}");
        assert!(!text.contains("absent"), "{text}");
        assert!(
            text.contains("inner: not trusted for the session") && text.contains("no chmod repair"),
            "an untrusted name is explained, not walked as trusted: {text}"
        );
        // The wrapper adds no repair of its own: the hints are those of the
        // ordinary tool alone (a temp directory's ancestors may add more, by platform).
        let alone = diagnose_exec_grants(&[refused("tool")], Some(bin.as_os_str()));
        assert_eq!(
            report.hints, alone.hints,
            "the wrapper gets no repair: {text}"
        );
    }

    /// #2739: no filesystem mutation without a positive operator decision.
    #[test]
    fn repair_requires_explicit_consent() {
        let mut report = ToolTrustReport::default();
        report.hints.push(TrustHint {
            path: "/brew/bin".into(),
            mode: 0o775,
            chmod_arg: "g-w",
        });
        let mut calls = 0;
        report.repair_with(false, |_| {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 0);
        report.repair_with(true, |_| {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 1);
    }
}
