//! `newt skills` — manage agentskills.io-format skills and share them across
//! harnesses (newt ↔ Claude Code ↔ Codex).
//!
//! A skill is the same `SKILL.md` folder everywhere, so "sharing" is just
//! placing that folder where each harness looks:
//!
//! - newt   → `~/.newt/skills/`
//! - Claude → `~/.claude/skills/` (built-in default; override via config/flag)
//! - Codex  → **no default** — set `[skills].codex_dir` or pass `--codex-dir`
//!
//! Copy is the default (independent duplicates); `--link` symlinks instead
//! (single source of truth, Unix only). The heavy lifting lives in
//! [`newt_skills::install_skill`]; this module is the CLI surface + directory
//! resolution.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use clap::{Subcommand, ValueEnum};
use newt_skills::InstallMode;

#[derive(Subcommand, Debug)]
pub enum SkillsCmd {
    /// List the skills installed in `~/.newt/skills`.
    List,
    /// Install a skill folder from a local path into `~/.newt/skills`.
    Install {
        /// Path to a skill folder (must contain `SKILL.md`).
        source: PathBuf,
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
    /// Share (export) a newt skill to Claude Code and/or Codex.
    Share {
        /// Skill name (a folder under `~/.newt/skills`).
        name: String,
        /// Which harness(es) to share to.
        #[arg(long, value_enum, default_value_t = Target::All)]
        to: Target,
        /// Override the Claude skills dir (default `~/.claude/skills`).
        #[arg(long)]
        claude_dir: Option<PathBuf>,
        /// Codex skills dir (no default — required to target Codex).
        #[arg(long)]
        codex_dir: Option<PathBuf>,
        /// Symlink instead of copying (Unix only).
        #[arg(long)]
        link: bool,
        /// Replace an existing destination.
        #[arg(long)]
        force: bool,
    },
    /// Adopt (import) a skill from Claude Code or Codex into newt.
    Adopt {
        /// Skill name (a folder under the source harness's skills dir).
        name: String,
        /// Which harness to adopt from.
        #[arg(long, value_enum)]
        from: Source,
        /// Override the Claude skills dir (default `~/.claude/skills`).
        #[arg(long)]
        claude_dir: Option<PathBuf>,
        /// Codex skills dir (no default — required to adopt from Codex).
        #[arg(long)]
        codex_dir: Option<PathBuf>,
        /// Symlink instead of copying (Unix only).
        #[arg(long)]
        link: bool,
        /// Replace an existing destination.
        #[arg(long)]
        force: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Claude,
    Codex,
    All,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

/// Entry point dispatched from `newt skills …`.
pub fn run(cmd: SkillsCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    let cfg = match config_path {
        Some(p) => newt_core::Config::load(p)?,
        None => newt_core::Config::resolve().unwrap_or_default(),
    };
    let dirs = HarnessDirs::resolve(&cfg);
    let newt_dir = newt_skills::default_skills_dir()
        .ok_or_else(|| anyhow!("could not resolve ~/.newt/skills (is $HOME set?)"))?;
    let mut out = std::io::stdout();
    run_with(cmd, &newt_dir, &dirs, &mut out)
}

/// The skills directory for each sibling harness, after applying the
/// flag > config > default precedence. `codex` is `None` when unconfigured.
#[derive(Debug, Clone, Default)]
struct HarnessDirs {
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
}

impl HarnessDirs {
    /// Resolve from config + built-in defaults (no CLI flags yet — those are
    /// folded in per-command by [`pick_dir`]).
    fn resolve(cfg: &newt_core::Config) -> Self {
        let skills = cfg.skills.clone().unwrap_or_default();
        Self {
            claude: pick_dir(None, skills.claude_dir.as_deref(), claude_default()),
            codex: pick_dir(None, skills.codex_dir.as_deref(), None),
        }
    }
}

/// Built-in Claude Code skills directory: `~/.claude/skills`.
fn claude_default() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("skills"))
}

/// Resolve a harness skills dir with precedence: CLI flag > config > default.
fn pick_dir(cli: Option<&Path>, cfg: Option<&str>, default: Option<PathBuf>) -> Option<PathBuf> {
    cli.map(PathBuf::from)
        .or_else(|| cfg.map(expand_tilde))
        .or(default)
}

/// Expand a leading `~/` to `$HOME` (config values are written by humans).
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
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

/// Core dispatch, parameterised over the newt dir, resolved harness dirs, and
/// an output sink so it can be exercised in tests against temp directories.
fn run_with(
    cmd: SkillsCmd,
    newt_dir: &Path,
    dirs: &HarnessDirs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    match cmd {
        SkillsCmd::List => cmd_list(newt_dir, out),
        SkillsCmd::Install {
            source,
            name,
            link,
            force,
        } => {
            let dest =
                newt_skills::install_skill(&source, newt_dir, name.as_deref(), mode(link), force)?;
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
            claude_dir,
            codex_dir,
            link,
            force,
        } => {
            let src = newt_dir.join(&name);
            if !src.join("SKILL.md").exists() {
                return Err(anyhow!(
                    "no skill '{name}' in {} — run `newt skills list`",
                    newt_dir.display()
                ));
            }
            let targets = share_targets(to, dirs, claude_dir.as_deref(), codex_dir.as_deref())?;
            for (label, dir) in targets {
                let dest = newt_skills::install_skill(&src, &dir, Some(&name), mode(link), force)
                    .with_context(|| format!("sharing '{name}' to {label}"))?;
                writeln!(
                    out,
                    "{} '{name}' to {label}: {}",
                    verb(mode(link)),
                    dest.display()
                )?;
            }
            Ok(())
        }
        SkillsCmd::Adopt {
            name,
            from,
            claude_dir,
            codex_dir,
            link,
            force,
        } => {
            let (label, src_root) =
                adopt_source(from, dirs, claude_dir.as_deref(), codex_dir.as_deref())?;
            let src = src_root.join(&name);
            if !src.join("SKILL.md").exists() {
                return Err(anyhow!(
                    "no skill '{name}' in {label} ({})",
                    src_root.display()
                ));
            }
            let dest = newt_skills::install_skill(&src, newt_dir, Some(&name), mode(link), force)?;
            writeln!(
                out,
                "{} '{name}' from {label} → {}",
                verb(mode(link)),
                dest.display()
            )?;
            Ok(())
        }
    }
}

fn cmd_list(newt_dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let skills = newt_skills::discover(newt_dir);
    if skills.is_empty() {
        writeln!(
            out,
            "No skills in {}. Add one with `newt skills install <path>` or `newt skills adopt`.",
            newt_dir.display()
        )?;
        return Ok(());
    }
    writeln!(out, "Skills in {}:", newt_dir.display())?;
    for s in &skills {
        writeln!(out, "  {}: {}", s.name, s.description)?;
    }
    Ok(())
}

/// Resolve the (label, dir) pairs a `share` should write to, applying CLI-flag
/// overrides and erroring when a requested harness has no directory.
fn share_targets(
    to: Target,
    dirs: &HarnessDirs,
    claude_cli: Option<&Path>,
    codex_cli: Option<&Path>,
) -> anyhow::Result<Vec<(&'static str, PathBuf)>> {
    let claude = pick_dir(claude_cli, None, dirs.claude.clone());
    let codex = pick_dir(codex_cli, None, dirs.codex.clone());
    let mut targets = Vec::new();
    match to {
        Target::Claude => targets.push(("claude", require(claude, "claude")?)),
        Target::Codex => targets.push(("codex", require(codex, "codex")?)),
        Target::All => {
            // `all` shares to every *configured* harness. Claude always has a
            // default; Codex is included only when configured, so a default
            // `--to all` doesn't fail just because Codex is unset.
            if let Some(d) = claude {
                targets.push(("claude", d));
            }
            if let Some(d) = codex {
                targets.push(("codex", d));
            }
            if targets.is_empty() {
                return Err(anyhow!("no harness directories resolved"));
            }
        }
    }
    Ok(targets)
}

fn adopt_source(
    from: Source,
    dirs: &HarnessDirs,
    claude_cli: Option<&Path>,
    codex_cli: Option<&Path>,
) -> anyhow::Result<(&'static str, PathBuf)> {
    match from {
        Source::Claude => Ok((
            "claude",
            require(pick_dir(claude_cli, None, dirs.claude.clone()), "claude")?,
        )),
        Source::Codex => Ok((
            "codex",
            require(pick_dir(codex_cli, None, dirs.codex.clone()), "codex")?,
        )),
    }
}

/// Turn an unresolved harness dir into a clear, actionable error.
fn require(dir: Option<PathBuf>, harness: &str) -> anyhow::Result<PathBuf> {
    dir.ok_or_else(|| {
        anyhow!(
            "{harness} skills directory is not configured — pass --{harness}-dir <DIR> \
             or set [skills].{harness}_dir in ~/.newt/config.toml"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_skill(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test {name}.\n---\nBody.\n"),
        )
        .unwrap();
    }

    fn dirs(claude: Option<PathBuf>, codex: Option<PathBuf>) -> HarnessDirs {
        HarnessDirs { claude, codex }
    }

    // --- pure resolution --------------------------------------------------

    #[test]
    fn pick_dir_precedence_flag_over_config_over_default() {
        let flag = PathBuf::from("/flag");
        let def = PathBuf::from("/default");
        assert_eq!(
            pick_dir(Some(&flag), Some("/cfg"), Some(def.clone())),
            Some(flag)
        );
        assert_eq!(
            pick_dir(None, Some("/cfg"), Some(def.clone())),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(pick_dir(None, None, Some(def.clone())), Some(def));
        assert_eq!(pick_dir(None, None, None), None);
    }

    #[test]
    fn expand_tilde_uses_home() {
        std::env::set_var("HOME", "/home/x");
        assert_eq!(
            expand_tilde("~/.codex/skills"),
            PathBuf::from("/home/x/.codex/skills")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn share_to_codex_without_dir_errors_clearly() {
        let err = share_targets(Target::Codex, &dirs(None, None), None, None).unwrap_err();
        assert!(err
            .to_string()
            .contains("codex skills directory is not configured"));
        assert!(err.to_string().contains("--codex-dir"));
    }

    #[test]
    fn share_all_skips_unconfigured_codex() {
        let targets = share_targets(
            Target::All,
            &dirs(Some(PathBuf::from("/c")), None),
            None,
            None,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "claude");
    }

    #[test]
    fn share_codex_cli_flag_supplies_missing_dir() {
        let cli = PathBuf::from("/tmp/codex");
        let targets = share_targets(Target::Codex, &dirs(None, None), None, Some(&cli)).unwrap();
        assert_eq!(targets, vec![("codex", cli)]);
    }

    // --- end-to-end dispatch against temp dirs ----------------------------

    #[test]
    fn share_copies_skill_into_target_dirs() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        let claude = tmp.path().join("claude");
        let codex = tmp.path().join("codex");
        make_skill(&newt, "commit-style");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Share {
                name: "commit-style".into(),
                to: Target::All,
                claude_dir: None,
                codex_dir: None,
                link: false,
                force: false,
            },
            &newt,
            &dirs(Some(claude.clone()), Some(codex.clone())),
            &mut out,
        )
        .unwrap();

        assert!(claude.join("commit-style").join("SKILL.md").is_file());
        assert!(codex.join("commit-style").join("SKILL.md").is_file());
        let log = String::from_utf8(out).unwrap();
        assert!(log.contains("copied 'commit-style' to claude"));
        assert!(log.contains("copied 'commit-style' to codex"));
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
                to: Target::Claude,
                claude_dir: Some(tmp.path().join("claude")),
                codex_dir: None,
                link: false,
                force: false,
            },
            &newt,
            &dirs(None, None),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no skill 'ghost'"));
    }

    #[test]
    fn adopt_imports_from_claude_into_newt() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        let claude = tmp.path().join("claude");
        make_skill(&claude, "judge");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Adopt {
                name: "judge".into(),
                from: Source::Claude,
                claude_dir: None,
                codex_dir: None,
                link: false,
                force: false,
            },
            &newt,
            &dirs(Some(claude), None),
            &mut out,
        )
        .unwrap();

        assert!(newt.join("judge").join("SKILL.md").is_file());
        assert!(String::from_utf8(out).unwrap().contains("from claude"));
    }

    #[test]
    fn install_from_local_path() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        let ext = tmp.path().join("external");
        make_skill(&ext, "release");

        let mut out = Vec::new();
        run_with(
            SkillsCmd::Install {
                source: ext.join("release"),
                name: None,
                link: false,
                force: false,
            },
            &newt,
            &dirs(None, None),
            &mut out,
        )
        .unwrap();
        assert!(newt.join("release").join("SKILL.md").is_file());
    }

    #[test]
    fn list_reports_empty_and_populated() {
        let tmp = tempdir().unwrap();
        let newt = tmp.path().join("newt");
        fs::create_dir_all(&newt).unwrap();

        let mut empty = Vec::new();
        cmd_list(&newt, &mut empty).unwrap();
        assert!(String::from_utf8(empty).unwrap().contains("No skills"));

        make_skill(&newt, "commit-style");
        let mut populated = Vec::new();
        cmd_list(&newt, &mut populated).unwrap();
        let log = String::from_utf8(populated).unwrap();
        assert!(log.contains("commit-style"));
    }
}
