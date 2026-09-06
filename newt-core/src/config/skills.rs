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
    /// The ordered skill-discovery search path, with `~/` expanded.
    ///
    /// Resolves `[skills].search` when configured; otherwise defaults to the
    /// single host-scoped `~/.newt/skills`. Order is preserved — earlier
    /// directories win on a name collision (see `newt_skills::discover_paths`).
    /// The default falls back to a relative `.newt/skills` only when `$HOME`
    /// can't be resolved, so the list is never empty.
    ///
    /// A configured `[skills].bundled_dir` is appended **last** (lowest
    /// priority), so a user skill of the same name shadows the bundled one.
    #[must_use]
    pub fn skill_search_dirs(&self) -> Vec<PathBuf> {
        let configured = self
            .skills
            .as_ref()
            .map(|s| s.search.as_slice())
            .unwrap_or(&[]);
        let mut dirs: Vec<PathBuf> = if configured.is_empty() {
            let default = Self::user_config_dir()
                .map(|dir| dir.join("skills"))
                .unwrap_or_else(|| PathBuf::from(".newt/skills"));
            vec![default]
        } else {
            configured.iter().map(|s| expand_tilde(s)).collect()
        };

        // Bundled skills scanned last: user-configured dirs win a name
        // collision (first-wins in `discover_paths`), so users can override
        // any bundled skill by shipping their own of the same name.
        if let Some(bundled) = self
            .skills
            .as_ref()
            .map(|s| s.bundled_dir.as_str())
            .filter(|s| !s.is_empty())
        {
            dirs.push(expand_tilde(bundled));
        }

        dirs
    }

    /// Fill in a default `[skills].bundled_dir` when the user left it unset, so
    /// an agent running **inside a newt checkout gets the repo's bundled skills
    /// surfaced out-of-the-box** (progressive-disclosure index → `use_skill`)
    /// without any config. Detection walks up from `cwd` for a
    /// `.newt/bundled-skills` directory; if none is found (or the field is
    /// already set), the config is returned unchanged. Kept off the pure
    /// [`Self::skill_search_dirs`] path — the filesystem probe lives only here.
    ///
    /// This is the smallest first step (dev/agent-in-checkout); packaging a
    /// default bundled dir for an *installed* newt is a follow-up (see the
    /// bundled-skills epic).
    #[must_use]
    pub fn with_bundled_default(mut self) -> Self {
        let already_set = self
            .skills
            .as_ref()
            .is_some_and(|s| !s.bundled_dir.is_empty());
        if already_set {
            return self;
        }
        let Ok(cwd) = std::env::current_dir() else {
            return self;
        };
        if let Some(dir) =
            find_ancestor_dir(&cwd, Path::new(".newt/bundled-skills"), |p| p.is_dir())
        {
            self.skills
                .get_or_insert_with(SkillsConfig::default)
                .bundled_dir = dir.to_string_lossy().into_owned();
        }
        self
    }
}
