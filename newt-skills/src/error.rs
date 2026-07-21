//! Typed errors, separated by concern.
//!
//! A library that returns one opaque error type forces every adopter to match
//! on error *strings*. These variants separate the three failures that call
//! for different handling:
//!
//! * **Authoring** — [`SkillError::Frontmatter`] / [`SkillError::Yaml`]: the
//!   `SKILL.md` is malformed. Actionable by whoever wrote the skill.
//! * **Identity** — [`SkillError::InvalidName`] / [`SkillError::UnknownSkill`]:
//!   the requested name is rejected or absent. [`SkillError::InvalidName`] is
//!   the **security boundary** — see [`crate::validate_skill_name`].
//! * **Environment** — [`SkillError::Io`] / [`SkillError::DestinationExists`] /
//!   [`SkillError::Unsupported`]: the filesystem said no.
//!
//! `SkillError` implements [`std::error::Error`], so `?` converts it into
//! `anyhow::Error` (or any `Box<dyn Error>`) for callers who prefer that.

use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, SkillError>;

/// Everything that can go wrong loading, discovering, or installing a skill.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SkillError {
    /// The `SKILL.md` frontmatter fence is missing or unterminated — a
    /// *structural* problem, before any YAML is parsed.
    #[error("SKILL.md frontmatter: {0}")]
    Frontmatter(&'static str),

    /// The frontmatter fence was found, but its YAML did not deserialize
    /// (missing `name`/`description`, duplicate keys, bad `caveats` shape, …).
    #[error("SKILL.md frontmatter is not valid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// A skill name is not a safe single path component. This is the
    /// capability boundary: a rejected name is one that could have escaped a
    /// search root or a destination root.
    #[error("invalid skill name {name:?}: {reason}")]
    InvalidName {
        /// The offending name, as supplied.
        name: String,
        /// Why it was rejected.
        reason: NameRejection,
    },

    /// No skill of that name exists anywhere on the search path.
    #[error("unknown skill: '{0}'")]
    UnknownSkill(String),

    /// A filesystem operation failed, tagged with the path it failed on.
    #[error("{path}: {source}")]
    Io {
        /// The path the operation was attempted against.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// An install destination already exists and `force` was not set.
    #[error("destination already exists: {0} (pass force to replace it)")]
    DestinationExists(PathBuf),

    /// The requested operation is not available on this platform.
    #[error("{0}")]
    Unsupported(&'static str),
}

impl SkillError {
    /// Build an [`SkillError::Io`] tagged with the path it happened on.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Why [`crate::validate_skill_name`] rejected a name.
///
/// Matchable so an adopter can distinguish "the operator typo'd" from "this
/// input was trying to escape the root".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameRejection {
    /// The name was empty.
    Empty,
    /// The name exceeded [`crate::MAX_NAME_LEN`] bytes.
    TooLong,
    /// The name began with `.` — reserved for hidden entries, which discovery
    /// skips (so such a skill could never be found anyway).
    Hidden,
    /// The name contained a path separator (`/` or `\`), so it addressed more
    /// than one directory level — or, if leading, an absolute path.
    PathSeparator,
    /// The name was `.` or `..`, i.e. a relative traversal component.
    Traversal,
    /// The name contained a character outside the permitted set (ASCII
    /// alphanumerics plus `-`, `_`, `.`), such as a control byte, a NUL, a
    /// shell metacharacter, or a bidi override.
    DisallowedCharacter,
}

impl std::fmt::Display for NameRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "must not be empty",
            Self::TooLong => "too long",
            Self::Hidden => "must not start with `.`",
            Self::PathSeparator => "must not contain a path separator",
            Self::Traversal => "must not be a `.`/`..` path traversal",
            Self::DisallowedCharacter => {
                "may only contain ASCII letters, digits, `-`, `_`, and `.`"
            }
        })
    }
}
