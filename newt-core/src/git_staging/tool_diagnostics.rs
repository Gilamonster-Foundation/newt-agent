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
