//! # newt-skills — agentskills.io-compatible skills for newt-agent
//!
//! A **skill** is procedural knowledge the agent loads *on demand*. Skills are
//! plain folders on disk so they stay portable across the whole Gilamonster
//! agent line (newt-agent, hermes-thoon) and Anthropic's Claude Code — anything
//! that speaks the [agentskills.io](https://agentskills.io) format:
//!
//! ```text
//! ~/.newt/skills/
//!   commit-style/
//!     SKILL.md          # YAML frontmatter + Markdown body
//!     template.txt      # optional bundled files (scripts, templates, …)
//!   release-checklist/
//!     SKILL.md
//! ```
//!
//! A `SKILL.md` is YAML frontmatter delimited by `---` lines, followed by a
//! Markdown body:
//!
//! ```text
//! ---
//! name: commit-style
//! description: How this project writes commit messages.
//! when_to_use: Before authoring any git commit in this repo.
//! version: 1.0.0
//! license: Apache-2.0
//! caveats:                 # OPTIONAL — agent-mesh Caveats serde shape
//!   exec: { only: ["git"] }
//!   fs_read: all
//!   max_calls: { at_most: 5 }
//! ---
//! Use the imperative mood. Wrap the body at 72 columns. …
//! ```
//!
//! ## Progressive disclosure
//!
//! The agent is shown only the **index** (one `name: description` line per
//! skill) in its system prompt. The full body loads only when the agent calls
//! the `use_skill` tool — see [`Skill::body`]. This keeps the prompt small no
//! matter how many skills are installed.
//!
//! ## Leash composition (MVP: parse-only)
//!
//! The optional `caveats` block is parsed into [`Skill::caveats`] but **not yet
//! enforced** in this MVP. Skill scripts run via newt's `run_command`, which is
//! already agent-bridle's confined shell governed by the session `[tui]`
//! permissions, so scripts are already leashed today. Meeting a skill's own
//! caveats into the session (`Caveats::meet`) when the skill loads is a
//! documented follow-up — see `docs/decisions/agent-skills.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// A set-valued capability axis, mirroring `agent_mesh_protocol::caveats::Scope`
/// for the parse-only frontmatter shape.
///
/// Logically identical to the canonical `Scope` (`all` is the top; `only`
/// authorizes exactly the listed items), so a SKILL.md `caveats` block maps
/// cleanly to the real lattice type when meet-enforcement lands (the documented
/// follow-up). A hand-written `Deserialize` accepts BOTH ergonomic YAML forms —
/// the bare string `all` and the single-key map `{ only: [...] }` — because
/// serde_yaml 0.9 serializes externally-tagged enums as YAML *tags* (`!only`),
/// which is awkward to author by hand in frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillScope {
    /// Unrestricted — authorizes every item (the `⊤` of this axis).
    All,
    /// Authorizes exactly the listed items.
    Only(BTreeSet<String>),
}

impl<'de> Deserialize<'de> for SkillScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ScopeVisitor;
        impl<'de> Visitor<'de> for ScopeVisitor {
            type Value = SkillScope;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the string `all` or a map `{ only: [..] }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<SkillScope, E> {
                match v {
                    "all" => Ok(SkillScope::All),
                    other => Err(de::Error::custom(format!(
                        "expected `all` or `{{ only: [..] }}`, got `{other}`"
                    ))),
                }
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<SkillScope, M::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("empty scope map"))?;
                if key != "only" {
                    return Err(de::Error::custom(format!(
                        "unknown scope key `{key}` (expected `only`)"
                    )));
                }
                let items: BTreeSet<String> = map.next_value()?;
                Ok(SkillScope::Only(items))
            }
        }
        deserializer.deserialize_any(ScopeVisitor)
    }
}

/// A numeric upper bound axis, mirroring
/// `agent_mesh_protocol::caveats::CountBound`. Like [`SkillScope`], a custom
/// `Deserialize` accepts the ergonomic YAML forms `unlimited` (bare string) and
/// `{ at_most: N }` (single-key map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillCountBound {
    /// No bound (the `⊤` of this axis).
    Unlimited,
    /// At most this many.
    AtMost(u64),
}

impl<'de> Deserialize<'de> for SkillCountBound {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CountVisitor;
        impl<'de> Visitor<'de> for CountVisitor {
            type Value = SkillCountBound;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the string `unlimited` or a map `{ at_most: N }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<SkillCountBound, E> {
                match v {
                    "unlimited" => Ok(SkillCountBound::Unlimited),
                    other => Err(de::Error::custom(format!(
                        "expected `unlimited` or `{{ at_most: N }}`, got `{other}`"
                    ))),
                }
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<SkillCountBound, M::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("empty count-bound map"))?;
                if key != "at_most" {
                    return Err(de::Error::custom(format!(
                        "unknown count-bound key `{key}` (expected `at_most`)"
                    )));
                }
                let n: u64 = map.next_value()?;
                Ok(SkillCountBound::AtMost(n))
            }
        }
        deserializer.deserialize_any(CountVisitor)
    }
}

/// The optional capability set a skill declares in its frontmatter `caveats`
/// block. Mirrors the field set of `agent_mesh_protocol::caveats::Caveats`
/// (`exec` / `fs_read` / `fs_write` / `net` / `max_calls`) so it can be `meet`'d
/// into the live session authority once enforcement lands. All fields default
/// to the top of their axis, so a partial block (e.g. just `exec`) is valid.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct SkillCaveats {
    /// Commands the skill's scripts may execute.
    #[serde(default)]
    pub exec: Option<SkillScope>,
    /// Filesystem paths the skill may read.
    #[serde(default)]
    pub fs_read: Option<SkillScope>,
    /// Filesystem paths the skill may write.
    #[serde(default)]
    pub fs_write: Option<SkillScope>,
    /// Network hosts the skill may reach.
    #[serde(default)]
    pub net: Option<SkillScope>,
    /// Upper bound on tool calls the skill is permitted.
    #[serde(default)]
    pub max_calls: Option<SkillCountBound>,
}

/// The YAML frontmatter of a `SKILL.md`, deserialized directly.
#[derive(Debug, Clone, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    /// `when_to_use` is canonical; `triggers` is accepted as an alias so skills
    /// authored for other harnesses still parse.
    #[serde(default, alias = "triggers")]
    when_to_use: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    caveats: Option<SkillCaveats>,
}

/// A loaded skill: its frontmatter metadata, Markdown body, source directory,
/// and any bundled sibling files.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Unique skill name (from frontmatter `name`).
    pub name: String,
    /// One-line description shown in the index (frontmatter `description`).
    pub description: String,
    /// When the agent should reach for this skill (frontmatter `when_to_use`
    /// or its `triggers` alias).
    pub when_to_use: Option<String>,
    /// Optional semantic version.
    pub version: Option<String>,
    /// Optional SPDX license id.
    pub license: Option<String>,
    /// The Markdown body — the procedural knowledge itself. Loaded on demand.
    pub body: String,
    /// The directory the skill lives in (`~/.newt/skills/<name>/`).
    pub dir: PathBuf,
    /// Bundled files alongside `SKILL.md` (scripts, templates, …).
    pub files: Vec<PathBuf>,
    /// Optional declared capability set — parsed, not yet enforced (follow-up).
    pub caveats: Option<SkillCaveats>,
}

impl Skill {
    /// Parse a `SKILL.md` document into its frontmatter fields + body.
    ///
    /// `dir` is the skill's directory (used to populate [`Skill::dir`]); pass an
    /// empty path when parsing standalone text. `files` is populated separately
    /// by [`discover`].
    ///
    /// # Errors
    /// Returns an error when the leading `---` frontmatter fence is missing or
    /// unterminated, or when the YAML is malformed / missing a required field.
    pub fn parse(text: &str, dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let (yaml, body) = split_frontmatter(text)?;
        let fm: Frontmatter = serde_yaml::from_str(yaml).context(
            "malformed SKILL.md frontmatter (expected YAML with `name` + `description`)",
        )?;
        Ok(Self {
            name: fm.name,
            description: fm.description,
            when_to_use: fm.when_to_use,
            version: fm.version,
            license: fm.license,
            body: body.trim_start_matches('\n').to_string(),
            dir: dir.into(),
            files: Vec::new(),
            caveats: fm.caveats,
        })
    }

    /// The one-line index entry for this skill: `name: description` plus
    /// `(when to use: …)` when present. Used to build the progressive-disclosure
    /// index injected into the system prompt — names + descriptions only, never
    /// the body.
    #[must_use]
    pub fn index_line(&self) -> String {
        match &self.when_to_use {
            Some(w) => format!("{}: {} (when to use: {})", self.name, self.description, w),
            None => format!("{}: {}", self.name, self.description),
        }
    }
}

/// Split a `SKILL.md` into `(frontmatter_yaml, markdown_body)`.
///
/// The document must open with a `---` line (a leading BOM / blank lines are
/// tolerated) and the frontmatter must be terminated by a closing `---` line.
/// Robust to a missing trailing newline after the closing fence.
fn split_frontmatter(text: &str) -> anyhow::Result<(&str, &str)> {
    let trimmed = text
        .trim_start_matches('\u{feff}')
        .trim_start_matches(['\n', '\r']);
    let rest = trimmed
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("SKILL.md must start with a `---` frontmatter fence"))?;
    // The opening fence is its own line; skip to the newline after it.
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .ok_or_else(|| anyhow!("SKILL.md `---` fence must be on its own line"))?;

    // Find the closing fence: a line that is exactly `---`.
    for (idx, line) in line_offsets(rest) {
        if line.trim_end() == "---" {
            let yaml = &rest[..idx];
            // Body begins after the closing fence line.
            let after = &rest[idx + line.len()..];
            return Ok((yaml, after));
        }
    }
    Err(anyhow!(
        "SKILL.md frontmatter is not terminated by a closing `---` line"
    ))
}

/// Yield `(byte_offset, line_with_terminator)` for each line in `s`.
fn line_offsets(s: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0usize;
    std::iter::from_fn(move || {
        if start >= s.len() {
            return None;
        }
        let rest = &s[start..];
        let len = match rest.find('\n') {
            Some(nl) => nl + 1,
            None => rest.len(),
        };
        let item = (start, &s[start..start + len]);
        start += len;
        Some(item)
    })
}

/// Resolve the host-scoped skills directory: `$HOME/.newt/skills`.
///
/// Skills are intentionally host-scoped (not per-workspace) — installed skills
/// are the operator's trusted procedural knowledge, available to every session.
#[must_use]
pub fn default_skills_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".newt").join("skills"))
}

/// Resolve the real `$HOME` (env first; no extra deps).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Discover all skills under `skills_dir`, scanning `<skills_dir>/*/SKILL.md`.
///
/// A missing directory is **not** an error — it yields an empty list (the
/// common "no skills installed" case). Bundled files (siblings of `SKILL.md`)
/// are listed on each [`Skill::files`]. Skills are returned sorted by name for
/// deterministic index ordering. Subdirectories without a readable, parseable
/// `SKILL.md` are skipped silently so one broken skill can't hide the rest.
pub fn discover(skills_dir: impl AsRef<Path>) -> Vec<Skill> {
    let skills_dir = skills_dir.as_ref();
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return Vec::new();
    };

    let mut skills: Vec<Skill> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = dir.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(mut skill) = Skill::parse(&text, &dir) else {
            continue;
        };
        skill.files = bundled_files(&dir);
        skills.push(skill);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// List the bundled sibling files of a skill (everything in `dir` except
/// `SKILL.md` itself and nested directories), sorted for determinism.
fn bundled_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n != "SKILL.md"))
        .collect();
    files.sort();
    files
}

/// Discover skills from the host-scoped default directory (`~/.newt/skills`).
/// Returns an empty list when `$HOME` is unset or the directory is absent.
#[must_use]
pub fn discover_default() -> Vec<Skill> {
    match default_skills_dir() {
        Some(dir) => discover(dir),
        None => Vec::new(),
    }
}

/// Build the progressive-disclosure index block for the system prompt, or
/// `None` when no skills are installed. ONLY names + descriptions (+ when_to_use)
/// — never bodies.
#[must_use]
pub fn index_block(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("Available skills (call `use_skill` to load one):\n");
    for s in skills {
        out.push_str("  ");
        out.push_str(&s.index_line());
        out.push('\n');
    }
    Some(out)
}

/// Load a single skill by name from `skills_dir` and return its full body plus
/// a list of its bundled file paths — the payload of the `use_skill` tool.
///
/// # Errors
/// Returns an error when no skill of that `name` exists.
pub fn load_body(skills_dir: impl AsRef<Path>, name: &str) -> anyhow::Result<String> {
    let skills = discover(skills_dir);
    let skill = skills
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("unknown skill: '{name}'"))?;

    let mut out = skill.body;
    if !skill.files.is_empty() {
        out.push_str("\n\nBundled files (read with read_file, run scripts via run_command):\n");
        for f in &skill.files {
            out.push_str("  ");
            out.push_str(&f.display().to_string());
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const VALID: &str = "---\nname: commit-style\ndescription: How this project writes commits.\nwhen_to_use: Before any git commit.\nversion: 1.0.0\nlicense: Apache-2.0\n---\nUse the imperative mood. Wrap at 72 columns.\n";

    #[test]
    fn parses_valid_skill_into_fields() {
        let s = Skill::parse(VALID, "/tmp/commit-style").unwrap();
        assert_eq!(s.name, "commit-style");
        assert_eq!(s.description, "How this project writes commits.");
        assert_eq!(s.when_to_use.as_deref(), Some("Before any git commit."));
        assert_eq!(s.version.as_deref(), Some("1.0.0"));
        assert_eq!(s.license.as_deref(), Some("Apache-2.0"));
        assert!(s.body.starts_with("Use the imperative mood."));
        assert!(s.caveats.is_none());
    }

    #[test]
    fn parses_optional_caveats_block() {
        let text = "---\nname: deployer\ndescription: Deploys.\ncaveats:\n  exec: { only: [\"git\", \"cargo\"] }\n  fs_read: all\n  max_calls: { at_most: 5 }\n---\nbody\n";
        let s = Skill::parse(text, "").unwrap();
        let cav = s.caveats.expect("caveats parsed");
        match cav.exec.unwrap() {
            SkillScope::Only(set) => {
                assert!(set.contains("git") && set.contains("cargo"));
            }
            SkillScope::All => panic!("expected Only"),
        }
        assert_eq!(cav.fs_read, Some(SkillScope::All));
        assert_eq!(cav.max_calls, Some(SkillCountBound::AtMost(5)));
    }

    #[test]
    fn triggers_alias_maps_to_when_to_use() {
        let text = "---\nname: x\ndescription: d\ntriggers: when stuck\n---\nbody\n";
        let s = Skill::parse(text, "").unwrap();
        assert_eq!(s.when_to_use.as_deref(), Some("when stuck"));
    }

    #[test]
    fn tolerates_missing_trailing_newline_after_fence() {
        let text = "---\nname: x\ndescription: d\n---";
        let s = Skill::parse(text, "").unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.body, "");
    }

    #[test]
    fn missing_opening_fence_is_clear_error() {
        let err = Skill::parse("no frontmatter here\n", "").unwrap_err();
        assert!(
            err.to_string().contains("must start with a `---`"),
            "got: {err}"
        );
    }

    #[test]
    fn unterminated_frontmatter_is_clear_error() {
        let err = Skill::parse("---\nname: x\ndescription: d\n", "").unwrap_err();
        assert!(err.to_string().contains("not terminated"), "got: {err}");
    }

    #[test]
    fn malformed_yaml_is_clear_error() {
        // Missing the required `description` field.
        let err = Skill::parse("---\nname: x\n---\nbody\n", "").unwrap_err();
        assert!(
            err.to_string().contains("malformed SKILL.md frontmatter"),
            "got: {err}"
        );
    }

    fn write_skill(root: &Path, name: &str, desc: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nBody of {name}.\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_finds_two_skills_and_lists_bundled_files() {
        let tmp = tempdir().unwrap();
        write_skill(tmp.path(), "alpha", "First skill");
        write_skill(tmp.path(), "beta", "Second skill");
        // A bundled script alongside beta's SKILL.md.
        fs::write(tmp.path().join("beta").join("deploy.sh"), "echo hi\n").unwrap();

        let skills = discover(tmp.path());
        assert_eq!(skills.len(), 2);
        // Sorted by name.
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "beta");
        assert!(skills[0].files.is_empty());
        assert_eq!(skills[1].files.len(), 1);
        assert!(skills[1].files[0].ends_with("deploy.sh"));
    }

    #[test]
    fn discover_missing_dir_is_empty_not_error() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(discover(&missing).is_empty());
    }

    #[test]
    fn discover_skips_broken_skill_keeps_good_one() {
        let tmp = tempdir().unwrap();
        write_skill(tmp.path(), "good", "Good one");
        let bad = tmp.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "not valid frontmatter").unwrap();

        let skills = discover(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn load_body_returns_body_for_known_skill() {
        let tmp = tempdir().unwrap();
        write_skill(tmp.path(), "alpha", "First skill");
        let body = load_body(tmp.path(), "alpha").unwrap();
        assert!(body.contains("Body of alpha."));
    }

    #[test]
    fn load_body_lists_bundled_files() {
        let tmp = tempdir().unwrap();
        write_skill(tmp.path(), "beta", "Second");
        fs::write(tmp.path().join("beta").join("deploy.sh"), "echo hi\n").unwrap();
        let body = load_body(tmp.path(), "beta").unwrap();
        assert!(body.contains("Bundled files"));
        assert!(body.contains("deploy.sh"));
    }

    #[test]
    fn load_body_unknown_skill_errors() {
        let tmp = tempdir().unwrap();
        let err = load_body(tmp.path(), "nope").unwrap_err();
        assert!(err.to_string().contains("unknown skill: 'nope'"));
    }

    #[test]
    fn index_block_lists_names_and_descriptions_only() {
        let tmp = tempdir().unwrap();
        write_skill(tmp.path(), "alpha", "First skill");
        let skills = discover(tmp.path());
        let block = index_block(&skills).unwrap();
        assert!(block.contains("Available skills (call `use_skill` to load one):"));
        assert!(block.contains("alpha: First skill"));
        // The body must NOT leak into the index (progressive disclosure).
        assert!(!block.contains("Body of alpha."));
    }

    #[test]
    fn index_block_empty_when_no_skills() {
        assert!(index_block(&[]).is_none());
    }
}
