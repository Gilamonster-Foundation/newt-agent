//! `newt new <ecosystem> [name]` — scaffold a project (#892).
//!
//! Materializes a [`ProjectTemplate`](newt_core::templates::ProjectTemplate)
//! (built-in `pyo3` / `python` / `rust`, or a `~/.newt/templates/*.toml`
//! drop-in) into a target directory, already wired for its lifecycle phases.
//! The name/dir resolution ([`resolve_target`]) is pure and unit-tested; the
//! filesystem write is a thin shell. Refuses to clobber a non-empty directory
//! unless `--force`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use newt_core::templates::{resolved_project_template, resolved_project_templates};
use newt_core::tooling::{resolved_phase_commands, Phase};

/// Run `newt new`. With no `ecosystem`, lists the available templates.
pub fn run(
    ecosystem: Option<String>,
    name: Option<String>,
    dir: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    let available: Vec<String> = resolved_project_templates()
        .into_iter()
        .map(|t| t.name)
        .collect();

    let Some(eco) = ecosystem else {
        println!("Available templates: {}", available.join(", "));
        println!("Usage: newt new <ecosystem> [name] [--dir DIR] [--force]");
        return Ok(());
    };

    let Some(template) = resolved_project_template(&eco) else {
        bail!(
            "unknown template '{eco}'. Available: {}",
            available.join(", ")
        );
    };

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let (target, project_name) = resolve_target(name.as_deref(), dir.as_deref(), &cwd)?;

    if dir_is_nonempty(&target) && !force {
        bail!(
            "target directory {} is not empty; pass --force to write into it",
            target.display()
        );
    }

    let files = template.render(&project_name);
    for (rel, body) in &files {
        let dest = target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&dest, body).with_context(|| format!("writing {}", dest.display()))?;
    }

    println!(
        "Scaffolded '{project_name}' ({eco}) in {} — {} file(s).",
        target.display(),
        files.len()
    );
    // Show the resolved lifecycle for the fresh project — proof it round-trips
    // with its tooling pack, and the human's next commands.
    print_next_steps(&target);
    Ok(())
}

/// Resolve the (target directory, project name) from the optional `name` and
/// `--dir`, defaulting relative paths against `cwd`. Pure — no filesystem — so it
/// is unit-tested with an injected `cwd`.
///
/// - name + dir → write into `dir`, call it `name`.
/// - name only → write into `cwd/name`.
/// - dir only → write into `dir`, name = the dir's final component.
/// - neither → error (a name is required).
fn resolve_target(
    name: Option<&str>,
    dir: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    let abs = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    match (name, dir) {
        (Some(n), Some(d)) => Ok((abs(d), n.to_string())),
        (Some(n), None) => Ok((cwd.join(n), n.to_string())),
        (None, Some(d)) => {
            let n = d
                .file_name()
                .and_then(|s| s.to_str())
                .context("target dir has no final component; pass a project name")?;
            Ok((abs(d), n.to_string()))
        }
        (None, None) => bail!("provide a project name: newt new <ecosystem> <name>"),
    }
}

/// A directory that exists and contains at least one entry. A missing directory
/// (the common case — we are about to create it) is NOT "non-empty".
fn dir_is_nonempty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Print the resolved `setup` / `check` lifecycle commands for the fresh project
/// (via the same [`resolved_phase_commands`] the crew + `lifecycle` tool use), so
/// the scaffold's round-trip with its tooling pack is visible.
fn print_next_steps(target: &Path) {
    let setup = resolved_phase_commands(target, Phase::Setup);
    let check = resolved_phase_commands(target, Phase::Check);
    if setup.is_empty() && check.is_empty() {
        return;
    }
    println!("Next (cd {}):", target.display());
    if let Some(s) = setup.first() {
        println!("  setup: {s}");
    }
    if let Some(c) = check.first() {
        println!("  check: {c}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_only_writes_under_cwd() {
        let cwd = Path::new("/work");
        let (target, name) = resolve_target(Some("mypkg"), None, cwd).unwrap();
        assert_eq!(target, PathBuf::from("/work/mypkg"));
        assert_eq!(name, "mypkg");
    }

    #[test]
    fn name_and_dir_uses_dir_and_name_independently() {
        let cwd = Path::new("/work");
        let (target, name) = resolve_target(Some("mypkg"), Some(Path::new("here")), cwd).unwrap();
        assert_eq!(target, PathBuf::from("/work/here"));
        assert_eq!(name, "mypkg");
    }

    #[test]
    fn dir_only_takes_name_from_final_component() {
        let cwd = Path::new("/work");
        let (target, name) = resolve_target(None, Some(Path::new("/abs/coolpkg")), cwd).unwrap();
        assert_eq!(target, PathBuf::from("/abs/coolpkg"));
        assert_eq!(name, "coolpkg");
    }

    #[test]
    fn neither_name_nor_dir_is_an_error() {
        assert!(resolve_target(None, None, Path::new("/work")).is_err());
    }
}
