//! The launch configuration a run started with.
//!
//! One object carrying the continuity mode a `newt` process was launched under,
//! its default, and a [`LaunchConfig::validate`] that **fails startup** when the
//! combination is not legal. A run that reaches the agentic loop has already
//! proved its configuration legal, so no code downstream needs to ask again.
//!
//! # Why this is a crate and not a `clap` attribute
//!
//! `clap`'s `conflicts_with` already enforces mutual exclusion for the CLI, and
//! the workspace uses it in half a dozen places. It buys nothing for a caller
//! that constructs a configuration **programmatically** — through pyO3, an
//! embedding host, or a test — because such a caller never passes through the
//! parser. Putting the rule only in the CLI would leave two consumers with two
//! different notions of what is legal, which is how one concept ends up with two
//! vocabularies that disagree.
//!
//! So the rule lives here, with no dependencies, where both reach it and neither
//! can route around it. `clap` may *also* declare `conflicts_with` for a better
//! error message at the terminal; that is a nicety layered on top, not the
//! enforcement.
//!
//! # Scope: continuity only
//!
//! This carries the launch decisions that have **no other home**. It deliberately
//! does not carry `max_rounds`: that value is resolved by
//! `solve_tool_round_limit(dc.max_tool_rounds, cli_tenacity(), args.max_rounds)`
//! with a precedence rule between tenacity and the explicit flag, and is captured
//! for the contract record at its own site. A second home for it here would be
//! the sprawl the reuse discipline exists to prevent.
//!
//! # Continuity is a mode, not a spectrum
//!
//! The host chooses whether prior session state is admitted at launch:
//!
//! | mode | what a run can inherit | a cap exit means |
//! |---|---|---|
//! | [`Continuity::Hermetic`] | no inherited session; explicitly admitted inputs | failure, always |
//! | [`Continuity::Resume`] | a parent frame, named by CID | paused, if a handoff was persisted |
//!
//! Hermetic runs start without inherited session state and require the host to
//! restrict ambient input sources. Records within the run still have causal
//! parents. This does not guarantee deterministic inference or external tools.
//! [`Continuity::Hermetic`] cannot carry a `resume_from`; [`LaunchConfig::validate`]
//! checks the combinations a type cannot express.

#![forbid(unsafe_code)]

use std::fmt;

/// How a run relates to work that came before it.
///
/// These are exclusive by construction rather than by convention — a value is
/// one or the other, so no code can hold "both" or "neither".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Continuity {
    /// Start without inherited session state and restrict inputs to sources
    /// admitted by the host. Records within this invocation may have parents.
    ///
    /// The lane benchmarks run in. A run that exhausts its budget here has no
    /// continuation available *by construction*, so a cap exit is a failure
    /// rather than a pause.
    Hermetic,

    /// Continue from a prior frame, named by its content id.
    ///
    /// `None` starts a resumable chain — a genesis frame that later runs may
    /// name as parent. `Some(cid)` continues from that frame.
    ///
    /// Reconstructing the inherited context is the frame layer's job
    /// (`agent-frame`), not this crate's. What is recorded here is only *which*
    /// frame, so a consumer can tell a resumed run from a replay.
    Resume { from: Option<String> },
}

impl Continuity {
    /// True when a run under this mode may inherit context from a prior frame.
    #[must_use]
    pub fn is_resumable(&self) -> bool {
        matches!(self, Self::Resume { .. })
    }

    /// The parent frame's content id, when continuing from one.
    ///
    /// `None` under [`Self::Hermetic`], and also `None` when starting a resumable
    /// chain. Parent presence identifies an actual resume, not whether the run
    /// permits later continuation. Use [`Self::is_resumable`] for that policy.
    #[must_use]
    pub fn parent_frame(&self) -> Option<&str> {
        match self {
            Self::Hermetic => None,
            Self::Resume { from } => from.as_deref(),
        }
    }

    /// The operations-facing name of the mode.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hermetic => "hermetic",
            Self::Resume { .. } => "resume",
        }
    }
}

impl Default for Continuity {
    /// Hermetic.
    ///
    /// Existing callers start without inherited session state. Smart-harness
    /// frontends may explicitly select a fresh resumable invocation instead.
    fn default() -> Self {
        Self::Hermetic
    }
}

/// Why a launch configuration was rejected.
///
/// Rendered through [`fmt::Display`] so callers can surface it as-is, and
/// carried as a typed value so a binding can map it without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// Both continuity modes were requested. They are exclusive: hermeticity
    /// means inheriting nothing, and resuming means inheriting something.
    ConflictingContinuity,

    /// A frame to resume from was named, but the run is hermetic and cannot
    /// inherit it.
    ResumeFromUnderHermetic { frame: String },

    /// `--resume-from` was given an empty or whitespace-only value.
    EmptyResumeFrom,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingContinuity => write!(
                f,
                "--hermetic and --resume are mutually exclusive: hermeticity means \
                 inheriting nothing, resuming means inheriting a prior frame. \
                 Choose one."
            ),
            Self::ResumeFromUnderHermetic { frame } => write!(
                f,
                "--resume-from {frame} was given, but this run is --hermetic and \
                 inherits nothing. Drop --hermetic to continue from that frame."
            ),
            Self::EmptyResumeFrom => write!(
                f,
                "--resume-from was given an empty value; it needs the content id \
                 of the frame to continue from"
            ),
        }
    }
}

impl std::error::Error for LaunchError {}

/// The launch decisions a run started with, after defaults are applied.
///
/// Construct with [`Self::default`] and adjust, or from a parser, then call
/// [`Self::validate`] **before** the run begins. A validated value is a promise
/// that the combination is legal; nothing downstream should re-check it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchConfig {
    /// How this run relates to prior work. See [`Continuity`].
    pub continuity: Continuity,
}

impl LaunchConfig {
    /// Reject a configuration that must not run.
    ///
    /// Call once, at startup, and fail the process on `Err`. The value is
    /// otherwise a standing claim that its combination is legal.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] for any combination this crate refuses; see that
    /// type for the cases.
    pub fn validate(&self) -> Result<(), LaunchError> {
        if let Continuity::Resume { from: Some(frame) } = &self.continuity {
            if frame.trim().is_empty() {
                return Err(LaunchError::EmptyResumeFrom);
            }
        }
        Ok(())
    }

    /// Build from the raw flag values a parser or binding collected, rejecting
    /// the combinations a [`Continuity`] value cannot itself express.
    ///
    /// This is the seam where "both flags given" is caught. Once a
    /// [`Continuity`] exists the conflict is unrepresentable, so it has to be
    /// refused here, on the way in.
    ///
    /// # Errors
    ///
    /// - [`LaunchError::ConflictingContinuity`] when both modes are requested.
    /// - [`LaunchError::ResumeFromUnderHermetic`] when a frame is named for a
    ///   hermetic run.
    /// - Anything [`Self::validate`] rejects.
    pub fn from_flags(
        hermetic: bool,
        resume: bool,
        resume_from: Option<String>,
    ) -> Result<Self, LaunchError> {
        if hermetic && resume {
            return Err(LaunchError::ConflictingContinuity);
        }
        if hermetic {
            if let Some(frame) = resume_from {
                return Err(LaunchError::ResumeFromUnderHermetic { frame });
            }
        }
        let continuity = if resume || resume_from.is_some() {
            Continuity::Resume { from: resume_from }
        } else {
            Continuity::Hermetic
        };
        let cfg = Self { continuity };
        cfg.validate()?;
        Ok(cfg)
    }

    /// One line naming what this run can and cannot inherit.
    ///
    /// Intended for the record a run emits about itself: a reader should be able
    /// to tell, without reading the invocation, whether a cap exit here was a
    /// failure or a pause.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.continuity {
            Continuity::Hermetic => {
                "hermetic: no inherited session, admitted inputs only, a cap exit is a failure"
                    .to_string()
            }
            Continuity::Resume { from: None } => {
                "resume: starts a resumable chain, no parent frame".to_string()
            }
            Continuity::Resume { from: Some(cid) } => {
                format!("resume: continues from frame {cid}")
            }
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
