//! Inspect signed approvals and prune only individually confirmed stale entries.
use anyhow::{Context, Result};
use newt_core::ocap_store::{self, CapabilityClass, PolicyFile};
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

pub(super) fn diagnose(config: Option<&newt_core::Config>, fix: bool) {
    println!("\nDurable grants:");
    if let Err(error) = inspect(config, fix) {
        println!("  FINDING: {error:#}");
    }
}

fn inspect(config: Option<&newt_core::Config>, fix: bool) -> Result<()> {
    let config_path = newt_core::Config::pinned_config_path()
        .or_else(newt_core::Config::user_config_path)
        .context("cannot resolve ~/.newt/ocap/approve.toml")?;
    let store = config_path.with_file_name("ocap").join("approve.toml");
    let text = match std::fs::read_to_string(&store) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let key = newt_identity::default_key_path()
        .ok()
        .and_then(|path| newt_identity::load_user_key(&path).ok());
    let (verified, warnings) =
        ocap_store::load_store(&config_path, key.map(|key| key.public().as_bytes()));
    for warning in warnings {
        println!("  FINDING: {warning}");
    }
    let file = PolicyFile::parse(&text).map_err(anyhow::Error::msg)?;
    let cwd = std::env::current_dir()?;
    if let Some(config) = config {
        match newt_tui::workspace_protection_for_diagnostics(config, &cwd) {
            Ok((mut policy, guard)) => {
                for (kind, target) in ocap_store::approved_grants(&verified) {
                    let candidate = newt_core::widen_caveats(&policy, &[(kind, target.clone())]);
                    match guard
                        .validate_request(kind, &target)
                        .and_then(|()| guard.validate_caveats(&candidate))
                    {
                        Ok(()) => policy = candidate,
                        Err(error) => {
                            println!("  FINDING: {target:?}: workspace protection: {error:#}");
                        }
                    }
                }
            }
            Err(error) => println!("  FINDING: workspace protection: {error:#}"),
        }
    } else {
        println!("  workspace protection unavailable: configuration did not resolve");
    }
    let entries = file
        .fs
        .iter()
        .enumerate()
        .map(|(index, entry)| (CapabilityClass::Fs, index, entry.path.as_str()))
        .chain(
            file.exec
                .iter()
                .enumerate()
                .map(|(index, entry)| (CapabilityClass::Exec, index, entry.target.as_str())),
        );
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let unavailable = repair_unavailable_reason();
    if fix {
        if let Some(reason) = unavailable {
            println!("  {reason}");
        }
    }
    let mut selected = Vec::new();
    let mut findings = 0;
    for (kind, index, target) in entries {
        let Some((reason, repairable)) = stale_target(kind, target) else {
            continue;
        };
        findings += 1;
        println!("  FINDING: {target:?}: {reason}");
        if fix && repairable && interactive && unavailable.is_none() && confirm(target) {
            selected.push((kind, index));
        }
    }
    if findings == 0 {
        println!("  no stale filesystem or executable targets");
    } else if fix && !interactive {
        println!("  --fix needs a terminal to choose — report only, nothing changed");
    }
    if !selected.is_empty() {
        let backup = prune(&config_path, &text, &selected)?;
        println!(
            "  pruned {} confirmed entries; backup: {backup:?}",
            selected.len()
        );
    }
    Ok(())
}

/// Basenames are symbolic command grants, not signed absolute names. Resolve
/// those with the same portable Brush PATH/PATHEXT lookup used by the shell.
fn stale_target(kind: CapabilityClass, target: &str) -> Option<(String, bool)> {
    let named = Path::new(target);
    if kind == CapabilityClass::Exec && !target.contains(['/', '\\']) && !named.has_root() {
        let paths = newt_core::exec_grants::dispatch_path().unwrap_or_default();
        return brush_core::pathsearch::resolve_command(std::env::split_paths(&paths), named)
            .is_none()
            .then(|| ("command does not exist on the dispatch PATH".into(), true));
    }
    match named.canonicalize() {
        Ok(resolved) if dunce::simplified(&resolved) != dunce::simplified(named) => Some((
            format!("target no longer resolves to its signed name (now {resolved:?})"),
            true,
        )),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(("target does not exist".into(), true))
        }
        Err(error) => Some((format!("cannot inspect target: {error}"), false)),
    }
}

fn repair_unavailable_reason() -> Option<&'static str> {
    cfg!(windows).then_some(
        "durable-grant --fix is report-only on Windows: owner-only backup access is not supported; nothing changed",
    )
}

fn confirm(target: &str) -> bool {
    let form = newt_core::interaction_form::confirm(
        format!("Prune stale durable grant {target:?}?"),
        "A dated backup is written first. Other entries keep their signatures; TOML comments remain in the backup.",
        "prune this entry", "keep this entry",
    );
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    newt_core::interaction_terminal::resolve_on_terminal(&window, &form)
        .is_some_and(|choice| choice.as_str() == newt_core::interaction_form::YES)
}

fn prune(config: &Path, expected: &str, selected: &[(CapabilityClass, usize)]) -> Result<PathBuf> {
    prune_with_backup_sync(
        config,
        expected,
        selected,
        newt_core::atomic_fs::sync_parent,
    )
}

fn prune_with_backup_sync(
    config: &Path,
    expected: &str,
    selected: &[(CapabilityClass, usize)],
    sync_backup: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    if let Some(reason) = repair_unavailable_reason() {
        anyhow::bail!(reason);
    }
    let (destination, _lock) = ocap_store::lock_approve_file(config)?;
    let current = std::fs::read_to_string(destination.as_path())?;
    anyhow::ensure!(
        current == expected,
        "approve.toml changed during confirmation; nothing pruned, rerun newt doctor --fix"
    );
    let mut file = PolicyFile::parse(&current).map_err(anyhow::Error::msg)?;
    // Remove from the back of each original array so a prior removal cannot
    // silently select its neighbor. Recheck target staleness under the lock.
    for &(kind, index) in selected.iter().rev() {
        let target = match kind {
            CapabilityClass::Fs => &file.fs.get(index).context("filesystem entry changed")?.path,
            CapabilityClass::Exec => {
                &file
                    .exec
                    .get(index)
                    .context("executable entry changed")?
                    .target
            }
            _ => anyhow::bail!("only filesystem and executable targets can be pruned"),
        };
        anyhow::ensure!(
            stale_target(kind, target).is_some_and(|(_, repairable)| repairable),
            "target is no longer stale; nothing pruned"
        );
        match kind {
            CapabilityClass::Fs => {
                file.fs.remove(index);
            }
            CapabilityClass::Exec => {
                file.exec.remove(index);
            }
            _ => unreachable!(),
        }
    }
    let repaired = file.to_toml().map_err(anyhow::Error::msg)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let backup = destination.as_path().with_file_name(format!(
        "approve.toml.backup-{}-{now}",
        newt_tui::probe::today_local_date()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut saved = options.open(&backup)?;
    saved.write_all(current.as_bytes())?;
    saved.sync_all()?;
    // Persist the backup's name before replacement can commit the pruned store.
    // The injected operation is the same parent-sync primitive used by atomic_fs.
    sync_backup(&backup).context("backup could not be durably published; nothing pruned")?;
    destination.atomic_write(repaired.as_bytes())?;
    Ok(backup)
}

#[cfg(test)]
#[path = "durable_grants_tests.rs"]
mod tests;
