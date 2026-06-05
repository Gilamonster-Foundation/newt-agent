//! `newt skills` — manage agentskills.io-format skills across a configurable
//! **search path**.
//!
//! A skill is the same `SKILL.md` folder in every harness (newt, Claude Code,
//! Codex, …). Cross-harness use is just a matter of *pointing newt at the
//! directories* via `[skills].search` in `~/.newt/config.toml`:
//!
//! ```toml
//! [skills]
//! search = ["~/.newt/skills", "~/.claude/skills", "~/.codex/skills"]
//! ```
//!
//! newt then reads the **union** of those dirs (first-directory-wins on a name
//! collision), so a skill authored in any harness is visible with no copying.
//!
//! Commands:
//! - `list` — show every skill on the search path (and flag shadowed copies).
//! - `install <path> [--into <dir>]` — copy/symlink an external skill folder
//!   into a skills dir (default: the first dir on the search path).
//! - `share <name> --to <dir>` — copy/symlink a skill into another dir, for the
//!   harnesses that only read their own (Claude/Codex won't read newt's dir).
//!
//! Copy is the default; `--link` symlinks (single source of truth, Unix only).
//! The filesystem work lives in [`newt_skills::install_skill`].

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Subcommand;
use newt_skills::InstallMode;

#[derive(Subcommand, Debug)]
pub enum SkillsCmd {
    /// List the skills found across the configured search path.
    List,
    /// Install a skill folder from a local path onto the search path.
    Install {
        /// Path to a skill folder (must contain `SKILL.md`).
        source: PathBuf,
        /// Destination skills dir (default: the first dir on the search path,
        /// normally `~/.newt/skills`).
        #[arg(long)]
        into: Option<PathBuf>,
        /// Destination folder name (defaults to the source folder name).
        #[arg(long)]
        name: Option<String>,
        /// Symlink instead of copying (single source of truth; Unix only).
        #[arg(long)]
        link: bool,
        /// Replace an existing destination.
        #[arg(long)]
        force: bool,
    },
    /// Share a skill into another directory (e.g. `~/.claude/skills`,
    /// `~/.codex/skills`) so a harness that only reads its own dir can see it.
    Share {
        /// Skill name (resolved across the search path, or under `--from`).
        name: String,
        /// Destination skills dir.
        #[arg(long)]
        to: PathBuf,
        /// Source skills dir (default: search the configured path).
        #[arg(long)]
        from: Option<PathBuf>,
        /// Symlink instead of copying (Unix only).
        #[arg(long)]
        link: bool,
        /// Replace an existing destination.
        #[arg(long)]
        force: bool,
    },
}

/// Entry point dispatched from `newt skills …`.
pub fn run(cmd: SkillsCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    let cfg = match config_path {
        Some(p) => newt_core::Config::load(p)?,
        None => newt_core::Config::resolve().unwrap_or_default(),
    };
    let search = cfg.skill_search_dirs();
    let mut out = std::io::stdout();
    run_with(cmd, &search, &mut out)
}

fn mode(link: bool) -> InstallMode {
    if link {
        InstallMode::Link
    } else {
        InstallMode::Copy
    }
}

/// The verb used in user-facing messages.
fn verb(mode: InstallMode) -> &'static str {
    match mode {
        InstallMode::Link => "linked",
        InstallMode::Copy => "copied",
    }
}

/// Core dispatch, parameterised over the resolved search path and an output
/// sink so it can be exercised in tests against temp directories.
fn run_with(cmd: SkillsCmd, search: &[PathBuf], out: &mut dyn Write) -> anyhow::Result<()> {
    match cmd {
        SkillsCmd::List => cmd_list(search, out),
        SkillsCmd::Install {
            source,
            into,
            name,
            link,
            force,
        } => {
            let dest_root = into
                .or_else(|| search.first().cloned())
                .ok_or_else(|| anyhow!("no destination — pass --into <dir>"))?;
            let dest = newt_skills::install_skill(
                &source,
                &dest_root,
                name.as_deref(),
                mode(link),
                force,
            )?;
            writeln!(
                out,
                "{} {} → {}",
                verb(mode(link)),
                source.display(),
                dest.display()
            )?;
            Ok(())
        }
        SkillsCmd::Share {
            name,
            to,
            from,
            link,
            force,
        } => {
            let src = match &from {
                Some(dir) => dir.join(&name),
                None => find_on_path(search, &name).ok_or_else(|| {
                    anyhow!("no skill '{name}' on the search path — run `newt skills list`")
                })?,
            };
            if !src.join("SKILL.md").exists() {
                return Err(anyhow!("no skill '{name}' in {}", src.display()));
            }
            let dest = newt_skills::install_skill(&src, &to, Some(&name), mode(link), force)?;
            writeln!(out, "{} '{name}' → {}", verb(mode(link)), dest.display())?;
            Ok(())
        }
    }
}

/// Resolve the folder of skill `name` on the search path (first dir wins).
fn find_on_path(search: &[PathBuf], name: &str) -> Option<PathBuf> {
    search
        .iter()
        .map(|d| d.join(name))
        .find(|p| p.join("SKILL.md").is_file())
}

fn cmd_list(search: &[PathBuf], out: &mut dyn Write) -> anyhow::Result<()> {
    let (skills, shadowed) = newt_skills::discover_paths_with_shadows(search);

    let path: Vec<String> = search.iter().map(|d| d.display().to_string()).collect();
    writeln!(out, "Search path: {}", path.join(", "))?;

    if skills.is_empty() {
        writeln!(
            out,
            "No skills found. Add one with `newt skills install <path>`, or add a \
             directory to [skills].search in ~/.newt/config.toml."
        )?;
    } else {
        writeln!(out, "Skills:")?;
        for s in &skills {
            // `s.dir` is the skill folder; its parent is the search dir it
            // came from — useful when several dirs are on the path.
            let from = s
                .dir
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            writeln!(out, "  {}: {}  [{from}]", s.name, s.description)?;
        }
    }

    // Surface shadowing rather than dropping duplicates silently.
    for s in &shadowed {
        writeln!(
            out,
            "  note: '{}' in {} is shadowed by an earlier directory on the search path",
            s.name,
            s.dir
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_skill(root: &Path, name: &str, desc: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nBody.\n"),
        )
        .unwrap();
    }

    #[test]
    fn install_defaults_into_first_search_dir() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        fs::create_dir_all(&newt).unwrap();
        let ext = tmp.path().join("external");
        make_skill(&ext, "release", "cut a release");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Install {
                source: ext.join("release"),
                into: None,
                name: None,
                link: false,
                force: false,
            },
            std::slice::from_ref(&newt),
            &mut out,
        )
        .unwrap();
        assert!(newt.join("release").join("SKILL.md").is_file());
    }

    #[test]
    fn install_into_explicit_dir() {
        let tmp = tempdir().unwrap();
        let ext = tmp.path().join("external");
        make_skill(&ext, "release", "cut a release");
        let target = tmp.path().join("elsewhere");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Install {
                source: ext.join("release"),
                into: Some(target.clone()),
                name: Some("renamed".into()),
                link: false,
                force: false,
            },
            &[tmp.path().join("newt")],
            &mut out,
        )
        .unwrap();
        assert!(target.join("renamed").join("SKILL.md").is_file());
    }

    #[test]
    fn share_resolves_source_from_search_path() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        make_skill(&newt, "commit-style", "how we commit");
        let claude = tmp.path().join("claude");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Share {
                name: "commit-style".into(),
                to: claude.clone(),
                from: None,
                link: false,
                force: false,
            },
            &[newt],
            &mut out,
        )
        .unwrap();
        assert!(claude.join("commit-style").join("SKILL.md").is_file());
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("copied 'commit-style'"));
    }

    #[test]
    fn share_with_explicit_from_dir() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        make_skill(&src, "judge", "scoring");
        let dst = tmp.path().join("dst");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Share {
                name: "judge".into(),
                to: dst.clone(),
                from: Some(src),
                link: false,
                force: false,
            },
            &[tmp.path().join("newt")], // search path doesn't contain it
            &mut out,
        )
        .unwrap();
        assert!(dst.join("judge").join("SKILL.md").is_file());
    }

    #[test]
    fn share_missing_skill_errors() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        fs::create_dir_all(&newt).unwrap();
        let mut out = Vec::new();
        let err = run_with(
            SkillsCmd::Share {
                name: "ghost".into(),
                to: tmp.path().join("claude"),
                from: None,
                link: false,
                force: false,
            },
            &[newt],
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no skill 'ghost'"));
    }

    #[test]
    fn list_reports_search_path_and_shadowing() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        let claude = tmp.path().join("claude");
        make_skill(&newt, "commit-style", "newt copy");
        make_skill(&claude, "commit-style", "claude copy"); // shadowed
        make_skill(&claude, "judge", "scoring"); // unique

        let mut out = Vec::new();
        cmd_list(&[newt.clone(), claude.clone()], &mut out).unwrap();
        let log = String::from_utf8(out).unwrap();

        assert!(log.contains("Search path:"));
        // First dir wins for the dup; the unique claude skill still listed.
        assert!(log.contains("commit-style: newt copy"));
        assert!(log.contains("judge: scoring"));
        // The shadowed claude copy is surfaced, not silently dropped.
        assert!(log.contains("is shadowed by an earlier directory"));
    }

    #[test]
    fn list_empty_search_path() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        fs::create_dir_all(&newt).unwrap();
        let mut out = Vec::new();
        cmd_list(&[newt], &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("No skills found"));
    }
}
