//! # newt-skills — agent skills with capability caveats
//!
//! A **skill** is procedural knowledge an agent loads *on demand*: a plain
//! folder on disk holding a `SKILL.md` (YAML frontmatter + Markdown body) and
//! any bundled files. That folder format is the [agentskills.io] convention, so
//! the same skill is portable across harnesses (Claude Code, Codex, the
//! Gilamonster agent line). This crate is the Rust reference for **discovering,
//! parsing, and installing** those folders.
//!
//! ## What makes this crate different: caveats
//!
//! An off-the-shelf `SKILL.md` is just a prompt fragment — knowledge with no
//! bound on what it may *do*. This crate extends the format with an optional
//! [`caveats`](SkillCaveats) block that attaches **object-capability
//! attenuation** to a skill: the commands it may run, the paths it may read or
//! write, the hosts it may reach, the number of tool calls it may make. Each
//! axis is a lattice value ([`SkillScope`] / [`SkillCountBound`]) whose top is
//! "unrestricted", so a skill can only ever *narrow* authority, never widen it.
//!
//! ```text
//! ---
//! name: deployer
//! description: Ship the release.
//! caveats:                       # object-capability attenuation
//!   exec: { only: [git, cargo] } # may run only git and cargo
//!   fs_write: { only: [dist] }   # may write only under dist/
//!   net: all                     # unrestricted network
//!   max_calls: { at_most: 5 }    # at most five tool calls
//! ---
//! Run the release checklist, then tag and push.
//! ```
//!
//! The caveats block is *parsed* here (see the example below); *enforcing* it —
//! meeting it into the live session's authority when the skill loads — is the
//! documented follow-up. Parsing it in this crate means the capability contract
//! travels **with** the skill, in the same portable file, rather than living in
//! harness-specific configuration. That is the crate's reason to exist:
//! *agentskills.io-compatible skills, with capability caveats.*
//!
//! ## Discovery, at a glance
//!
//! ```text
//! ~/.newt/skills/
//!   commit-style/
//!     SKILL.md          # frontmatter + body
//!     template.txt      # optional bundled files
//!   release-checklist/
//!     SKILL.md
//! ```
//!
//! [`discover`] scans one root; [`discover_paths`] scans an ordered search path
//! and resolves name collisions **earlier-directory-wins** (see its docs for
//! the full precedence rules). [`index_block`] renders the
//! *progressive-disclosure index* — one `name: description` line per skill —
//! that goes in the system prompt, so the prompt stays small no matter how many
//! skills are installed; the full body loads only when the agent asks for it by
//! name via [`load_body_from`].
//!
//! ## Names are a security boundary
//!
//! A skill name selects a folder, and the name of the skill an agent asks to
//! load is *model-controlled input*. [`validate_skill_name`] rejects anything
//! that is not a single safe path component — traversal (`..`), path
//! separators, hidden `.`-names, control bytes — so a skill name can never
//! escape a search root or an install destination. Discovery drops skills whose
//! declared name fails that check, and [`load_body`] / [`load_body_from`]
//! reject an unsafe request before touching the filesystem.
//!
//! ## Worked example
//!
//! Parsing a `SKILL.md` and reading its capability caveats:
//!
//! ```
//! use newt_skills::{Skill, SkillScope, SkillCountBound};
//!
//! let doc = "\
//! ---
//! name: deployer
//! description: Ship the release.
//! caveats:
//!   exec: { only: [git, cargo] }
//!   net: all
//!   max_calls: { at_most: 5 }
//! ---
//! Run the release checklist.";
//!
//! let skill = Skill::parse(doc, "").expect("valid SKILL.md");
//! assert_eq!(skill.name, "deployer");
//!
//! let caveats = skill.caveats.expect("declared caveats");
//! assert_eq!(caveats.net, Some(SkillScope::All));
//! assert_eq!(caveats.max_calls, Some(SkillCountBound::AtMost(5)));
//! match caveats.exec.unwrap() {
//!     SkillScope::Only(cmds) => assert!(cmds.contains("git")),
//!     SkillScope::All => unreachable!(),
//! }
//! ```
//!
//! ## Errors
//!
//! Every fallible entry point returns [`SkillError`], a typed enum separated by
//! concern (frontmatter/YAML authoring vs. name/identity vs. filesystem
//! environment) so an embedder can react to each without string-matching. It
//! implements [`std::error::Error`], so `?` still lifts it into `anyhow` or any
//! `Box<dyn Error>`.
//!
//! [agentskills.io]: https://agentskills.io

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

mod error;
mod fs;

pub use error::{NameRejection, Result, SkillError};
pub use fs::{OsFs, SkillFs};

// ---------------------------------------------------------------------------
// Skill names — the capability boundary
// ---------------------------------------------------------------------------

/// Maximum byte length of a skill name. Names are single path components; this
/// bound keeps a pathological name from becoming a resource concern.
pub const MAX_NAME_LEN: usize = 128;

/// Validate that `name` is a safe single path component.
///
/// A skill name is used to select a folder — both when an agent asks to load a
/// skill by name and when a skill is installed under a destination root — and
/// that name is frequently model-controlled input. This is the crate's
/// path-traversal guard: it rejects anything that could address more than one
/// directory level or escape upward.
///
/// Accepts a non-empty string of at most [`MAX_NAME_LEN`] bytes containing only
/// ASCII letters, digits, `-`, `_`, and `.`, that is not `.`/`..`, does not
/// start with `.`, and contains no path separator. The returned
/// [`NameRejection`] says which rule failed.
///
/// # Errors
/// Returns [`SkillError::InvalidName`] when `name` violates any rule above.
pub fn validate_skill_name(name: &str) -> Result<()> {
    let bad = |reason| SkillError::InvalidName {
        name: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(bad(NameRejection::Empty));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(bad(NameRejection::TooLong));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(bad(NameRejection::PathSeparator));
    }
    if name == "." || name == ".." {
        return Err(bad(NameRejection::Traversal));
    }
    if name.starts_with('.') {
        return Err(bad(NameRejection::Hidden));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(bad(NameRejection::DisallowedCharacter));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Caveats — the capability-attenuation frontmatter
// ---------------------------------------------------------------------------

/// A set-valued capability axis, mirroring `agent_mesh_protocol::caveats::Scope`
/// for the frontmatter shape.
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
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ScopeVisitor;
        impl<'de> Visitor<'de> for ScopeVisitor {
            type Value = SkillScope;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the string `all` or a map `{ only: [..] }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<SkillScope, E> {
                match v {
                    "all" => Ok(SkillScope::All),
                    other => Err(de::Error::custom(format!(
                        "expected `all` or `{{ only: [..] }}`, got `{other}`"
                    ))),
                }
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<SkillScope, M::Error> {
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
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct CountVisitor;
        impl<'de> Visitor<'de> for CountVisitor {
            type Value = SkillCountBound;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the string `unlimited` or a map `{ at_most: N }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<SkillCountBound, E> {
                match v {
                    "unlimited" => Ok(SkillCountBound::Unlimited),
                    other => Err(de::Error::custom(format!(
                        "expected `unlimited` or `{{ at_most: N }}`, got `{other}`"
                    ))),
                }
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<SkillCountBound, M::Error> {
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

// ---------------------------------------------------------------------------
// Frontmatter + Skill
// ---------------------------------------------------------------------------

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
    /// Returns [`SkillError::Frontmatter`] when the leading `---` fence is
    /// missing or unterminated, or [`SkillError::Yaml`] when the frontmatter is
    /// not valid YAML (malformed, missing a required field, or duplicate keys).
    pub fn parse(text: &str, dir: impl Into<PathBuf>) -> Result<Self> {
        let (yaml, body) = split_frontmatter(text)?;
        let fm: Frontmatter = serde_yaml::from_str(yaml)?;
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
fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let trimmed = text
        .trim_start_matches('\u{feff}')
        .trim_start_matches(['\n', '\r']);
    let rest = trimmed.strip_prefix("---").ok_or(SkillError::Frontmatter(
        "must start with a `---` frontmatter fence",
    ))?;
    // The opening fence is its own line; skip to the newline after it.
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .ok_or(SkillError::Frontmatter(
            "`---` fence must be on its own line",
        ))?;

    // Find the closing fence: a line that is exactly `---`.
    for (idx, line) in line_offsets(rest) {
        if line.trim_end() == "---" {
            let yaml = &rest[..idx];
            // Body begins after the closing fence line.
            let after = &rest[idx + line.len()..];
            return Ok((yaml, after));
        }
    }
    Err(SkillError::Frontmatter(
        "is not terminated by a closing `---` line",
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

// ---------------------------------------------------------------------------
// Default location
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Whether a directory-entry name is hidden (starts with `.`). Discovery skips
/// hidden entries — both skill folders (`.git`, `.DS_Store` dirs) and bundled
/// files (`.DS_Store`, editor swap files) — so incidental dotfiles never leak
/// into the index or a skill's file list.
fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Discover all skills under `skills_dir`, scanning `<skills_dir>/*/SKILL.md`.
///
/// A missing or unreadable directory is **not** an error — it yields an empty
/// list (the common "no skills installed" case). Hidden entries (names starting
/// with `.`) are skipped. Bundled files (siblings of `SKILL.md`) are listed on
/// each [`Skill::files`]. Skills are returned sorted by name for a deterministic
/// index. A subdirectory without a readable, parseable `SKILL.md` — or one whose
/// declared name fails [`validate_skill_name`] — is skipped silently so one
/// broken skill can't hide the rest.
pub fn discover(skills_dir: impl AsRef<Path>) -> Vec<Skill> {
    discover_in(&OsFs, skills_dir.as_ref())
}

/// Discovery core, parameterised over the [`SkillFs`] seam so the logic is
/// testable without touching disk. [`discover`] passes [`OsFs`].
fn discover_in(fs: &dyn SkillFs, skills_dir: &Path) -> Vec<Skill> {
    let Ok(names) = fs.read_dir_names(skills_dir) else {
        return Vec::new();
    };
    let mut skills: Vec<Skill> = Vec::new();
    for name in names {
        if is_hidden(&name) {
            continue;
        }
        let dir = skills_dir.join(&name);
        if !fs.is_dir(&dir) {
            continue;
        }
        let Ok(text) = fs.read_to_string(&dir.join("SKILL.md")) else {
            continue;
        };
        let Ok(mut skill) = Skill::parse(&text, &dir) else {
            continue;
        };
        // Keep traversal / garbage names out of the index: a name that could
        // not be safely *loaded* must never be *shown* either.
        if validate_skill_name(&skill.name).is_err() {
            continue;
        }
        skill.files = bundled_files_in(fs, &dir);
        skills.push(skill);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// List the bundled sibling files of a skill (regular files in `dir` except
/// `SKILL.md` and hidden dotfiles), sorted for determinism.
fn bundled_files_in(fs: &dyn SkillFs, dir: &Path) -> Vec<PathBuf> {
    let Ok(names) = fs.read_dir_names(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = names
        .into_iter()
        .filter(|n| !is_hidden(n) && n.as_os_str() != OsStr::new("SKILL.md"))
        .map(|n| dir.join(n))
        .filter(|p| fs.is_file(p))
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

/// Discover skills across an ordered **search path** of directories — the union
/// of every `<dir>/*/SKILL.md`, deduplicated by name with **earlier
/// directories winning** a collision (so a newt-owned or project-local skill
/// shadows one of the same name found later in the path). Missing directories
/// are skipped. The result is sorted by name for a deterministic index,
/// matching [`discover`].
///
/// Use [`discover_paths_with_shadows`] when you need to *report* which
/// duplicates were shadowed (e.g. `newt skills list`).
pub fn discover_paths(dirs: &[impl AsRef<Path>]) -> Vec<Skill> {
    discover_paths_with_shadows(dirs).0
}

/// Like [`discover_paths`], but also returns the **shadowed** duplicates: skills
/// that lost a name collision to an earlier directory. Each shadowed entry is
/// the losing [`Skill`] (its [`Skill::dir`] tells you where it came from), so a
/// caller can warn "`<name>` in <dir> is shadowed by an earlier copy".
///
/// ## Precedence, precisely
/// Directories are consulted left to right. The **first** occurrence of a name
/// wins and lands in the winners list; every later occurrence of that same name
/// is a shadow, in path order. Within a single directory two folders cannot
/// collide (a directory has one entry per name), so ties only ever cross
/// directories, and the earlier directory always wins.
pub fn discover_paths_with_shadows(dirs: &[impl AsRef<Path>]) -> (Vec<Skill>, Vec<Skill>) {
    let dirs: Vec<PathBuf> = dirs.iter().map(|d| d.as_ref().to_path_buf()).collect();
    discover_paths_in(&OsFs, &dirs)
}

/// Search-path discovery core, over the [`SkillFs`] seam. See
/// [`discover_paths_with_shadows`].
fn discover_paths_in(fs: &dyn SkillFs, dirs: &[PathBuf]) -> (Vec<Skill>, Vec<Skill>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut winners: Vec<Skill> = Vec::new();
    let mut shadowed: Vec<Skill> = Vec::new();
    for dir in dirs {
        for skill in discover_in(fs, dir) {
            // First occurrence (earliest dir in the path) wins; any later
            // skill of the same name is shadowed, never silently merged.
            if seen.insert(skill.name.clone()) {
                winners.push(skill);
            } else {
                shadowed.push(skill);
            }
        }
    }
    winners.sort_by(|a, b| a.name.cmp(&b.name));
    (winners, shadowed)
}

// ---------------------------------------------------------------------------
// Body loading
// ---------------------------------------------------------------------------

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

/// Render a skill's loadable payload: its body plus, if any, a trailing list of
/// its bundled file paths — the payload of the `use_skill` tool.
fn render_body(skill: Skill) -> String {
    let mut out = skill.body;
    if !skill.files.is_empty() {
        out.push_str("\n\nBundled files (read with read_file, run scripts via run_command):\n");
        for f in &skill.files {
            out.push_str("  ");
            out.push_str(&f.display().to_string());
            out.push('\n');
        }
    }
    out
}

/// Load a single skill by name from `skills_dir` and return its full body plus
/// a list of its bundled file paths — the payload of the `use_skill` tool.
///
/// Resolves by the skill's declared frontmatter `name`, matching what
/// [`discover`] / [`index_block`] show, rather than by folder name.
///
/// # Errors
/// Returns [`SkillError::InvalidName`] when `name` is not a safe path component,
/// or [`SkillError::UnknownSkill`] when no skill of that name exists.
pub fn load_body(skills_dir: impl AsRef<Path>, name: &str) -> Result<String> {
    load_body_in(&OsFs, skills_dir.as_ref(), name)
}

fn load_body_in(fs: &dyn SkillFs, skills_dir: &Path, name: &str) -> Result<String> {
    validate_skill_name(name)?;
    let skill = discover_in(fs, skills_dir)
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| SkillError::UnknownSkill(name.to_string()))?;
    Ok(render_body(skill))
}

/// Load a skill body by name across an ordered search path, honouring the same
/// **earlier-directory-wins** precedence as [`discover_paths`].
///
/// The request `name` is validated up front ([`validate_skill_name`]) — it is
/// model-controlled input — so an unsafe name is rejected before any filesystem
/// access, and resolution is by declared frontmatter `name` (so what the agent
/// was shown in the index is exactly what loads).
///
/// # Errors
/// Returns [`SkillError::InvalidName`] for an unsafe `name`, or
/// [`SkillError::UnknownSkill`] when no directory in the path declares a skill
/// of that name.
pub fn load_body_from(dirs: &[impl AsRef<Path>], name: &str) -> Result<String> {
    let dirs: Vec<PathBuf> = dirs.iter().map(|d| d.as_ref().to_path_buf()).collect();
    load_body_from_in(&OsFs, &dirs, name)
}

fn load_body_from_in(fs: &dyn SkillFs, dirs: &[PathBuf], name: &str) -> Result<String> {
    validate_skill_name(name)?;
    for dir in dirs {
        if let Some(skill) = discover_in(fs, dir).into_iter().find(|s| s.name == name) {
            return Ok(render_body(skill));
        }
    }
    Err(SkillError::UnknownSkill(name.to_string()))
}

// ---------------------------------------------------------------------------
// Install / share / adopt — moving a skill folder between harness directories
// ---------------------------------------------------------------------------

/// How a skill folder is materialised at its destination.
///
/// A skill is the same `SKILL.md`-format folder everywhere (newt, Claude Code,
/// Codex), so "sharing" a skill across harnesses is just placing that folder
/// where each harness looks — either an independent [`Copy`](InstallMode::Copy)
/// or a single-source [`Link`](InstallMode::Link).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Recursively copy the skill folder — an independent duplicate. Edits in
    /// one harness do not propagate to the others. Symlinks inside the folder
    /// are recreated verbatim (never followed out of the tree).
    Copy,
    /// Symlink the destination at the source — a single source of truth, so an
    /// edit is seen by every harness. **Unix only** (returns an error
    /// elsewhere); copy is the portable default.
    Link,
}

/// Install the skill folder `src` into `dest_root/<name>`.
///
/// This is the one primitive behind `newt skills install` (local path →
/// `~/.newt/skills`), `share` (newt → Claude/Codex), and `adopt`
/// (Claude/Codex → newt): they differ only in which `src`/`dest_root` they
/// pass.
///
/// `src` must be a directory containing a parseable `SKILL.md`. `name`
/// overrides the destination folder name (defaults to `src`'s folder name). The
/// resolved folder name is validated with [`validate_skill_name`], so the
/// destination can never escape `dest_root`. With [`InstallMode::Copy`] the
/// whole folder (SKILL.md + bundled files) is copied recursively — any symlink
/// inside is recreated as a symlink, not followed; with [`InstallMode::Link`] a
/// symlink `dest -> src` is created. An existing destination is an error unless
/// `force` (which replaces it). Returns the destination path.
///
/// # Errors
/// Returns [`SkillError::Io`] when `src` has no readable `SKILL.md`,
/// [`SkillError::Frontmatter`] / [`SkillError::Yaml`] when it is unparseable,
/// [`SkillError::InvalidName`] when the resolved folder name is unsafe,
/// [`SkillError::DestinationExists`] when the destination exists and `force` is
/// false, [`SkillError::Unsupported`] for [`InstallMode::Link`] off Unix, or
/// [`SkillError::Io`] when a filesystem operation fails.
pub fn install_skill(
    src: &Path,
    dest_root: &Path,
    name: Option<&str>,
    mode: InstallMode,
    force: bool,
) -> Result<PathBuf> {
    // Validate the source really is a skill before touching the destination.
    let manifest = src.join("SKILL.md");
    let text = std::fs::read_to_string(&manifest).map_err(|e| SkillError::io(&manifest, e))?;
    let parsed = Skill::parse(&text, src)?;

    let folder = match name {
        Some(n) => n.to_string(),
        None => src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| parsed.name.clone()),
    };
    // Security: the resolved folder name is joined onto dest_root, so it must be
    // a single safe component — otherwise `--name ../x` could escape.
    validate_skill_name(&folder)?;
    let dest = dest_root.join(&folder);

    if dest.exists() || std::fs::symlink_metadata(&dest).is_ok() {
        if !force {
            return Err(SkillError::DestinationExists(dest));
        }
        remove_path(&dest)?;
    }
    std::fs::create_dir_all(dest_root).map_err(|e| SkillError::io(dest_root, e))?;

    match mode {
        InstallMode::Copy => copy_dir(src, &dest)?,
        InstallMode::Link => symlink_dir(src, &dest)?,
    }
    Ok(dest)
}

/// Remove a path whether it's a symlink, file, or directory.
fn remove_path(p: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(p).map_err(|e| SkillError::io(p, e))?;
    let outcome = if meta.file_type().is_symlink() || meta.is_file() {
        std::fs::remove_file(p)
    } else {
        std::fs::remove_dir_all(p)
    };
    outcome.map_err(|e| SkillError::io(p, e))
}

/// Recursively copy directory `src` into `dest` (created if absent). Regular
/// files are copied; real subdirectories recursed; symlinks recreated verbatim
/// (never followed out of the tree, so a symlink loop cannot cause unbounded
/// recursion and a link escaping the folder is not inlined); other node types
/// (fifos, sockets, devices) are skipped.
fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| SkillError::io(dest, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| SkillError::io(src, e))? {
        let entry = entry.map_err(|e| SkillError::io(src, e))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let ft = entry.file_type().map_err(|e| SkillError::io(&from, e))?;
        if ft.is_symlink() {
            copy_symlink(&from, &to)?;
        } else if ft.is_dir() {
            copy_dir(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to).map_err(|e| SkillError::io(&from, e))?;
        }
        // Other node types (fifo/socket/device) are intentionally skipped —
        // a portable skill folder should not carry them.
    }
    Ok(())
}

/// Recreate the symlink at `from` as a symlink at `to`, verbatim (same target).
/// Unix only; on other platforms the link is skipped so the copy still
/// succeeds.
#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    let target = std::fs::read_link(from).map_err(|e| SkillError::io(from, e))?;
    std::os::unix::fs::symlink(&target, to).map_err(|e| SkillError::io(to, e))
}

#[cfg(not(unix))]
fn copy_symlink(_from: &Path, _to: &Path) -> Result<()> {
    // No portable symlink primitive; skip so the copy still completes.
    Ok(())
}

/// Symlink `dest` → `src` (absolute, so it survives a CWD change). Unix only.
#[cfg(unix)]
fn symlink_dir(src: &Path, dest: &Path) -> Result<()> {
    let abs = std::fs::canonicalize(src).map_err(|e| SkillError::io(src, e))?;
    std::os::unix::fs::symlink(&abs, dest).map_err(|e| SkillError::io(dest, e))
}

#[cfg(not(unix))]
fn symlink_dir(_src: &Path, _dest: &Path) -> Result<()> {
    Err(SkillError::Unsupported(
        "--link (symlink) is only supported on Unix; re-run without --link to copy",
    ))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
