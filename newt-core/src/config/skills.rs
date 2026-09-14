//! Ordered skill discovery paths and checkout-bundled defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{expand_tilde, find_ancestor_dir, Config};

// ---------------------------------------------------------------------------
// Skill search path
// ---------------------------------------------------------------------------

/// The skill discovery **search path**: an ordered list of directories newt
/// scans for agentskills.io-format `SKILL.md` folders.
///
/// A skill is the same folder in every harness, so cross-harness use is just a
/// matter of *pointing newt at the directories* — list `~/.claude/skills`,
/// `~/.codex/skills`, a project-local `.skills/`, whatever — and their skills
/// become visible with no copying. The list is open-ended on purpose: there is
/// no hard-coded knowledge of any particular harness. Earlier entries win on a
/// name collision.
///
/// Example `~/.newt/config.toml`:
/// ```toml
/// [skills]
/// search = ["~/.newt/skills", "~/.claude/skills", "~/.codex/skills"]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Ordered directories to scan for skills. Empty → `~/.newt/skills`.
    /// `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,

    /// Directory of bundled skills shipped with newt-agent. Scanned *after* the
    /// user's `search` paths — i.e. at the **lowest** priority — so a user skill
    /// of the same name shadows the bundled one (earlier directories win a
    /// collision; see [`newt_skills::discover_paths`]). Empty → no bundled
    /// directory is scanned. `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bundled_dir: String,
}

impl Config {
    /// The EFFECTIVE skill search path, with `~/` expanded — the ONE resolver
    /// every skill consumer uses (#2331): the prompt index, the `use_skill`
    /// loader, posture skills, persona binding checks, and `newt skills`.
    /// Consumers that resolved their own list drifted apart: a bundled-only
    /// skill was indexed and then `unknown skill` to the loader.
    ///
    /// Resolves `[skills].search` when configured; otherwise the single
    /// host-scoped [`Self::skill_install_dir`]. Order is preserved — earlier
    /// directories win on a name collision (see `newt_skills::discover_paths`).
    /// The default falls back to a relative `.newt/skills` only when neither
    /// `$NEWT_CONFIG_DIR` nor `$HOME` resolves, so the list is never empty.
    ///
    /// The bundled directory is appended **last** (lowest priority), so a
    /// user skill of the same name shadows the bundled one: `[skills].bundled_dir`
    /// when set, otherwise the `.newt/bundled-skills` of a newt checkout found
    /// by walking up from the process cwd, so an agent running inside a
    /// checkout gets the repo's bundled skills with no config. Packaging a
    /// default for an *installed* newt is a follow-up (bundled-skills epic).
    #[must_use]
    pub fn skill_search_dirs(&self) -> Vec<PathBuf> {
        let cwd = std::env::current_dir().ok();
        self.skill_search_dirs_with(cwd.as_deref(), |p| p.is_dir())
    }

    /// Where a skill is installed or seeded by default: the first configured
    /// `[skills].search` entry, else `$NEWT_CONFIG_DIR/skills` or
    /// `~/.newt/skills`. Never the bundled directory, and `None` rather than a
    /// cwd-relative guess when no root resolves — a write must not land in
    /// whatever directory newt happened to start in.
    #[must_use]
    pub fn skill_install_dir(&self) -> Option<PathBuf> {
        match self.skills.as_ref().and_then(|s| s.search.first()) {
            Some(first) => Some(expand_tilde(first)),
            None => Self::user_config_dir().map(|dir| dir.join("skills")),
        }
    }

    /// [`Self::skill_search_dirs`] with the checkout probe injected, so the
    /// ordering and fallback rules are testable without touching disk.
    pub(super) fn skill_search_dirs_with(
        &self,
        cwd: Option<&Path>,
        is_dir: impl Fn(&Path) -> bool,
    ) -> Vec<PathBuf> {
        let skills = self.skills.as_ref();
        let mut dirs: Vec<PathBuf> = match skills.filter(|s| !s.search.is_empty()) {
            Some(s) => s.search.iter().map(|d| expand_tilde(d)).collect(),
            None => vec![self
                .skill_install_dir()
                .unwrap_or_else(|| PathBuf::from(".newt/skills"))],
        };
        let bundled = skills
            .map(|s| s.bundled_dir.as_str())
            .filter(|d| !d.is_empty())
            .map(expand_tilde)
            .or_else(|| {
                cwd.and_then(|cwd| {
                    find_ancestor_dir(cwd, Path::new(".newt/bundled-skills"), &is_dir)
                })
            });
        dirs.extend(bundled);
        dirs
    }
}
