//! `RoleProfile` — promotes a persona from a prompt-only overlay into a
//! "role profile" that binds a system-prompt overlay together with a toolset,
//! a capability (caveat) profile, and backend/router policy.
//!
//! ## Why this exists
//!
//! A persona today is just a system-prompt overlay loaded from a `.md` file
//! (see `newt-tui`'s `Persona`). The one-airframe-many-roles topology — a
//! single newt binary that can embody a `dragon-rider` (orchestrator), a
//! `wing-commander` (arbiter/judge), or a `worker` — needs each role to carry
//! *more* than prose: which tools it may call, what authority (caveats) it
//! holds, and which model/tier it should route to. This module is that seam.
//!
//! ## File format (backward compatible)
//!
//! A role profile is still a plain `.md` file. The only addition is an
//! **optional** TOML front-matter block fenced by `+++` lines at the very top
//! of the file:
//!
//! ```text
//! +++
//! role = "worker"
//! tools = ["read_file", "write_file", "run_command"]
//! model = "qwen2.5-coder:14b"
//! tier = "STANDARD"
//!
//! [caveats]
//! fs_read = "all"
//! fs_write = ["src/"]
//! exec = ["cargo"]
//! net = "none"
//! max_calls = 40
//! +++
//!
//! # Worker
//!
//! You edit files and run builds...
//! ```
//!
//! A file with **no** `+++` front-matter parses into a `RoleProfile` whose
//! every field except `prompt` is `None` — i.e. exactly today's prompt-only
//! persona. This is the load-bearing backward-compat guarantee.
//!
//! ## What this slice wires vs. follow-ups
//!
//! This module only *parses and represents* a role profile and converts the
//! caveat shape into the canonical [`Caveats`]. It does **not** enforce the
//! toolset or caveats — enforcement (an agent-bridle registry at the tool
//! dispatch site, worker/MCP entry points reading the profile) is a deliberate
//! follow-up. See `docs/design/role-profiles.md`.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::initiative::Initiative;
use newt_policy::preset::CaveatProfile;
use newt_router::Tier;

/// A parsed role profile: a system-prompt overlay plus the optional toolset /
/// capability / router policy declared in TOML front-matter.
///
/// `prompt` is always the markdown body (front-matter stripped). Every other
/// field is `Some` only when the front-matter declared it; a profile with no
/// front-matter has all-`None` non-prompt fields and behaves exactly like
/// today's prompt-only persona.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleProfile {
    /// The markdown body (front-matter removed). Injected as the persona
    /// overlay exactly as today.
    pub prompt: String,
    /// Declared role name (e.g. `"dragon-rider"`, `"worker"`). Independent of
    /// the persona file's stem, so a file can name the role it embodies.
    pub role: Option<String>,
    /// Preferred tools for an interactive persona; `None` means no preference.
    /// Dedicated worker/server entrypoints may apply this as a role boundary.
    pub tools: Option<Vec<String>>,
    /// Skill names (matching a `SKILL.md` `name` under `skill_search_dirs`)
    /// this role binds on activation (FR-4, #1041). `None` = no bound
    /// skills — today's behavior, unaffected.
    pub skills: Option<Vec<String>>,
    /// Declared capability profile. Advisory for interactive personas; explicit
    /// worker/server role bindings may use it as an authority ceiling.
    pub caveats: Option<CaveatProfile>,
    /// Preferred backend model id (router policy hint).
    pub model: Option<String>,
    /// Preferred router tier (router policy hint).
    // INERT-CODE-RATCHET: F14 WIRE: role tier is displayed but never participates in backend placement.
    pub tier: Option<Tier>,
    /// Operating ALTITUDE — act (doer, the default) vs advise (coach). Selected
    /// in front-matter as `altitude = "coach"`. `None` = doer. Drives which base
    /// identity the system prompt installs (FR-5, #999): a `Coach` altitude
    /// REPLACES `DEFAULT_SOUL` with `COACH_SOUL` so a coaching persona doesn't
    /// ship two contradictory identities (doer soul + advise overlay).
    pub altitude: Option<Altitude>,
    /// BACKEND — names a `[[backends]]` entry this persona runs on (e.g.
    /// `"sol"` → gpt-5.6-sol). `None` = the session's configured backend.
    /// Auto-selection (overriding `default_backend` while this persona is
    /// active) is a follow-up; declaring it is wired here first.
    pub backend: Option<String>,
    /// COGNITION — per-call reasoning depth (the psyche dial). `None` = backend
    /// default. The backend projects this semantic level onto its own request
    /// controls.
    pub cognition: Option<Cognition>,
    /// INITIATIVE — how much this persona looks before acting. `None` = the
    /// model-family / config default. (Persona tenacity no longer exists:
    /// tenacity is set only explicitly.)
    pub initiative: Option<Initiative>,
    /// CREW — when `true`, this persona runs with the multi-agent crew (`/team`)
    /// on by default. `None`/`false` = single-agent. Enforcement is a follow-up.
    pub crew: Option<bool>,
    /// Operator-selected communication style. Unspecified traits inherit the
    /// persona/backend's style; these levels never confer tool authority.
    pub personality: Option<PersonalityTraits>,
}

/// An operator-selected style level, not a psychological measurement.
/// Construction and deserialization both enforce the inclusive 0..=100 range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8")]
pub struct PersonalityLevel(u8);

impl PersonalityLevel {
    /// The validated integer selected by the operator.
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for PersonalityLevel {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err("personality levels must be integers from 0 through 100")
        }
    }
}

/// The shared ordering and names used by the persona parser and style controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonalityTrait {
    Agreeableness,
    Extraversion,
    Warmth,
    Approachability,
    ProsocialBehavior,
}

impl PersonalityTrait {
    pub const ALL: [Self; 5] = [
        Self::Agreeableness,
        Self::Extraversion,
        Self::Warmth,
        Self::Approachability,
        Self::ProsocialBehavior,
    ];

    /// The stable TOML field name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Agreeableness => "agreeableness",
            Self::Extraversion => "extraversion",
            Self::Warmth => "warmth",
            Self::Approachability => "approachability",
            Self::ProsocialBehavior => "prosocial_behavior",
        }
    }

    /// The operator-facing name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ProsocialBehavior => "prosocial behavior",
            _ => self.key(),
        }
    }

    /// The lower/higher communication behavior this operator preference selects.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Agreeableness => "Lower: candid/challenging; higher: collaborative framing, without agreeing with incorrect claims.",
            Self::Extraversion => "Lower: reserved; higher: conversational, without unsolicited actions.",
            Self::Warmth => "Lower: neutral; higher: reassuring.",
            Self::Approachability => "Lower: formal; higher: accessible.",
            Self::ProsocialBehavior => "Lower: task-focused; higher: considering affected people/helpfulness, still no unauthorized acts.",
        }
    }
}

/// Optional style selections carried by the existing role-profile record.
/// `None` is unspecified, not an implicit midpoint. These are communication
/// preferences only: neither authority nor honesty is adjustable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PersonalityTraits {
    pub agreeableness: Option<PersonalityLevel>,
    pub extraversion: Option<PersonalityLevel>,
    pub warmth: Option<PersonalityLevel>,
    pub approachability: Option<PersonalityLevel>,
    pub prosocial_behavior: Option<PersonalityLevel>,
}

impl PersonalityTraits {
    #[must_use]
    pub fn get(&self, kind: PersonalityTrait) -> Option<PersonalityLevel> {
        match kind {
            PersonalityTrait::Agreeableness => self.agreeableness,
            PersonalityTrait::Extraversion => self.extraversion,
            PersonalityTrait::Warmth => self.warmth,
            PersonalityTrait::Approachability => self.approachability,
            PersonalityTrait::ProsocialBehavior => self.prosocial_behavior,
        }
    }

    pub fn set(&mut self, kind: PersonalityTrait, level: Option<PersonalityLevel>) {
        let slot = match kind {
            PersonalityTrait::Agreeableness => &mut self.agreeableness,
            PersonalityTrait::Extraversion => &mut self.extraversion,
            PersonalityTrait::Warmth => &mut self.warmth,
            PersonalityTrait::Approachability => &mut self.approachability,
            PersonalityTrait::ProsocialBehavior => &mut self.prosocial_behavior,
        };
        *slot = level;
    }

    /// Resolve only unspecified axes from the inherited persona. Explicit zero
    /// is a selection, not an inheritance sentinel.
    #[must_use]
    pub fn resolve(mut self, inherited: Option<Self>) -> Self {
        if let Some(inherited) = inherited {
            for kind in PersonalityTrait::ALL {
                self.set(kind, self.get(kind).or(inherited.get(kind)));
            }
        }
        self
    }

    /// One communication-style overlay, omitted entirely when no axis is set.
    /// The same canonical trait vocabulary drives both controls and rendering.
    #[must_use]
    pub fn prompt_block(&self) -> String {
        let lines: Vec<_> = PersonalityTrait::ALL
            .into_iter()
            .filter_map(|kind| {
                self.get(kind).map(|level| {
                    format!(
                        "- {}: {}/100\n  {}",
                        kind.label(),
                        level.get(),
                        kind.describe()
                    )
                })
            })
            .collect();
        if lines.is_empty() {
            return String::new();
        }
        format!(
            "## Communication style\nOperator-selected communication-style levels (0-100):\n{}\n\n\
             These are communication-style preferences, not psychological measurements. They do not change tool authority, tenacity, or honesty. Remain civil and respectful even at low levels.",
            lines.join("\n")
        )
    }
}

impl ContentAddressable for PersonalityTraits {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// The persona's operating altitude (FR-5, #999). Decides whether the base
/// identity ("soul") is the doer `DEFAULT_SOUL` or the advise-first
/// `COACH_SOUL`. Serialized in front-matter as `altitude = "coach"` (or the
/// synonym `"advise"`); an absent field is [`Altitude::Doer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Altitude {
    /// Act: make the change, run the command. Today's behavior.
    #[default]
    Doer,
    /// Advise: explain, present options, recommend — never mutate. Accepts the
    /// synonym `"advise"` in front-matter.
    #[serde(alias = "advise")]
    Coach,
}

/// COGNITION — the psyche's reasoning-depth dial: how hard the model thinks
/// per call. Declared in front-matter as `cognition = "thoughtful"`; an
/// absent field leaves the backend default. Responses projects it onto
/// `reasoning.effort`; an explicitly capable Chat Completions endpoint projects
/// it onto a local generation policy.
///
/// The ladder runs from the least reasoning the endpoint allows to the most
/// (paired with tenacity and crew under the `obsessive` posture). The
/// pre-rename labels (`glancing` … `contemplating`) are the one-time
/// importer's job ([`crate::psyche_import`]), never accepted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Cognition {
    /// The least reasoning the endpoint allows. Wire: `reasoning.effort = minimal`.
    Zen,
    /// Short reasoning before acting. Wire: `low`.
    Rational,
    /// Weighs the options — the default. Wire: `medium`.
    #[default]
    Thoughtful,
    /// The deepest reasoning. Wire: `high`.
    Meticulous,
}

impl Cognition {
    /// The OpenAI `reasoning.effort` value this level maps to on the Responses
    /// wire (emitted by `agentic::responses_reasoning_field`). Chat Completions
    /// uses the backend-neutral generation policy instead and only when the
    /// endpoint explicitly advertises support.
    #[must_use]
    pub fn reasoning_effort(self) -> &'static str {
        crate::model_card::ReasoningEffort::from(self).as_str()
    }

    /// Stable lowercase label — the front-matter / `/cognition` spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Zen => "zen",
            Self::Rational => "rational",
            Self::Thoughtful => "thoughtful",
            Self::Meticulous => "meticulous",
        }
    }

    /// One-line backend-neutral description for `/cognition` listings.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Zen => "least reasoning the endpoint allows (minimal)",
            Self::Rational => "short reasoning before acting (low)",
            Self::Thoughtful => "weighs the options (medium)",
            Self::Meticulous => "deepest reasoning (high)",
        }
    }

    /// All levels, light → deep (for `/cognition list` and menus).
    #[must_use]
    pub fn all() -> [Self; 4] {
        [
            Self::Zen,
            Self::Rational,
            Self::Thoughtful,
            Self::Meticulous,
        ]
    }
}

impl std::fmt::Display for Cognition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for Cognition {
    type Err = String;

    /// Accepts the current labels and `default`. A pre-rename label is an
    /// error that names its replacement — never silently accepted.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        if s == "default" {
            return Ok(Self::default());
        }
        if let Some(level) = Self::all().into_iter().find(|c| c.label() == s) {
            return Ok(level);
        }
        if let Some(new) = crate::psyche_import::renamed_cognition(&s) {
            return Err(format!("cognition '{s}' was renamed to '{new}'"));
        }
        let levels: Vec<&str> = Self::all().into_iter().map(Self::label).collect();
        Err(format!("unknown cognition '{s}' ({})", levels.join("|")))
    }
}

/// The TOML front-matter shape. Kept separate from [`RoleProfile`] so the
/// `prompt` (markdown body) is never part of the serde surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct FrontMatter {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    skills: Option<Vec<String>>,
    #[serde(default)]
    caveats: Option<CaveatProfile>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tier: Option<Tier>,
    #[serde(default)]
    altitude: Option<Altitude>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    cognition: Option<Cognition>,
    #[serde(default)]
    initiative: Option<Initiative>,
    #[serde(default)]
    crew: Option<bool>,
    #[serde(default)]
    personality: Option<PersonalityTraits>,
}

impl RoleProfile {
    /// Parse a role-profile `.md` file's full text into a [`RoleProfile`].
    ///
    /// If the text begins with a `+++` fence line, everything up to the closing
    /// `+++` line is parsed as TOML front-matter and the remainder is the
    /// prompt body. Otherwise the whole text is the prompt and every other
    /// field is `None` (prompt-only — today's behavior).
    ///
    /// # Errors
    ///
    /// Returns an error if a front-matter fence is opened but never closed, or
    /// if the front-matter is not valid TOML for the expected shape.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        // The envelope grammar moved to `crate::markup` (A1, #1825); role
        // profiles are its first consumer. Error text preserved verbatim.
        let split = crate::markup::split_newt_metadata(text).map_err(|e| match e {
            crate::markup::EnvelopeError::Unclosed => {
                anyhow::anyhow!("role-profile front-matter opened with `+++` but never closed")
            }
            other => anyhow::anyhow!("role-profile front-matter: {other}"),
        })?;
        let (front_matter, body) = (split.front_matter, split.body);
        let body = body.trim().to_string();
        let Some(fm_text) = front_matter else {
            // No front-matter: prompt-only, exactly today's persona.
            return Ok(Self {
                prompt: body,
                ..Self::default()
            });
        };
        let fm: FrontMatter = toml::from_str(fm_text)
            .map_err(|e| anyhow::anyhow!("invalid role-profile front-matter: {e}"))?;
        Ok(Self {
            prompt: body,
            role: fm.role,
            tools: fm.tools,
            skills: fm.skills,
            caveats: fm.caveats,
            model: fm.model,
            tier: fm.tier,
            altitude: fm.altitude,
            backend: fm.backend,
            cognition: fm.cognition,
            initiative: fm.initiative,
            crew: fm.crew,
            personality: fm.personality,
        })
    }

    /// Serialize all known metadata and the prompt through the existing TOML
    /// envelope. Preserves parsed semantics, not source comments/formatting.
    /// An empty envelope keeps a metadata-like prompt body unambiguous.
    ///
    /// # Errors
    ///
    /// Returns an error if the front-matter cannot be serialized as TOML or
    /// would exceed the envelope limit or contain a closing-fence line.
    pub fn to_markdown(&self) -> anyhow::Result<String> {
        let front_matter = FrontMatter {
            role: self.role.clone(),
            tools: self.tools.clone(),
            skills: self.skills.clone(),
            caveats: self.caveats.clone(),
            model: self.model.clone(),
            tier: self.tier,
            altitude: self.altitude,
            backend: self.backend.clone(),
            cognition: self.cognition,
            initiative: self.initiative,
            crew: self.crew,
            personality: self.personality,
        };
        crate::markup::assemble_newt_metadata(
            &toml::to_string(&front_matter)?,
            &format!("\n{}\n", self.prompt),
        )
        .map_err(Into::into)
    }

    /// `true` when this profile declares more than a prompt (i.e. front-matter
    /// bound a role, tools, skills, caveats, model, tier, altitude, or any
    /// psyche dial). A prompt-only persona returns `false`.
    #[must_use]
    pub fn is_role_bound(&self) -> bool {
        self.role.is_some()
            || self.tools.is_some()
            || self.skills.is_some()
            || self.caveats.is_some()
            || self.model.is_some()
            || self.tier.is_some()
            || self.altitude.is_some()
            || self.backend.is_some()
            || self.cognition.is_some()
            || self.initiative.is_some()
            || self.crew.is_some()
            || self.personality.is_some()
    }

    /// Load a named persona `.md` file from `dir` (#1021 PR 5.2 — the
    /// headless counterpart of `newt-tui`'s `PersonaStore::load`).
    ///
    /// Deliberately does **not** seed a shipped default when the file is
    /// missing — `PersonaStore`'s per-file-idempotent seeding (FR-16, #1000)
    /// is correct first-run UX for an interactive session, but a stdio
    /// server silently writing files into `~/.newt/personas/` on every
    /// startup is a footgun under CI/systemd. A headless caller with an
    /// unresolvable `--persona <name>` should fail loudly instead. An existing
    /// file with pre-rename labels is rewritten once by
    /// [`crate::psyche_import::read_persona_file`], like every persona read.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` isn't a valid persona name (letters,
    /// numbers, `-`, `_` only — the same rule `PersonaStore` enforces, kept
    /// in sync deliberately rather than shared, since crossing the
    /// `newt-core`/`newt-tui` boundary for an 8-line check isn't worth the
    /// coupling), if the file doesn't exist, or if it fails to parse.
    pub fn load_from_dir(
        name: &str,
        dir: &std::path::Path,
        report: &mut dyn FnMut(crate::tty::Notice<'static>),
    ) -> anyhow::Result<Self> {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("persona names may only contain letters, numbers, '-' and '_'");
        }
        let path = dir.join(format!("{name}.md"));
        let raw = crate::psyche_import::read_persona_file(&path, report).map_err(|e| {
            anyhow::anyhow!("persona `{name}` not found at {}: {e}", path.display())
        })?;
        let profile = Self::parse(&raw).map_err(|e| anyhow::anyhow!("persona `{name}`: {e}"))?;
        if profile.prompt.is_empty() {
            anyhow::bail!("persona `{name}` is empty: {}", path.display());
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CountBound, Scope};

    /// The shipped personas must preserve the old explicit persona nudge.
    /// This is distinct from the obsessive command, which holds initiative.
    #[test]
    fn psyche_split_review_bundled_personas_preserve_action_timing() {
        for (name, text) in [
            ("bob", include_str!("../../personas/bob.md")),
            ("obsessive", include_str!("../../personas/obsessive.md")),
        ] {
            assert_eq!(
                RoleProfile::parse(text).unwrap().initiative,
                Some(Initiative::Eager),
                "{name}"
            );
        }
    }

    #[test]
    fn no_front_matter_is_prompt_only() {
        let text = "# Coder\n\nYou write code.\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Coder\n\nYou write code.");
        assert_eq!(rp.role, None);
        assert_eq!(rp.tools, None);
        assert_eq!(rp.caveats, None);
        assert_eq!(rp.model, None);
        assert_eq!(rp.tier, None);
        assert!(!rp.is_role_bound());
    }

    #[test]
    fn parses_full_front_matter() {
        let text = "\
+++
role = \"worker\"
tools = [\"read_file\", \"write_file\", \"run_command\"]
model = \"qwen2.5-coder:14b\"
tier = \"STANDARD\"

[caveats]
fs_read = \"all\"
fs_write = [\"src/\"]
exec = [\"cargo\"]
net = \"none\"
max_calls = 40
+++

# Worker

You edit files and run builds.
";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Worker\n\nYou edit files and run builds.");
        assert_eq!(rp.role.as_deref(), Some("worker"));
        assert_eq!(
            rp.tools,
            Some(vec![
                "read_file".to_string(),
                "write_file".to_string(),
                "run_command".to_string(),
            ])
        );
        assert_eq!(rp.model.as_deref(), Some("qwen2.5-coder:14b"));
        assert_eq!(rp.tier, Some(Tier::Standard));
        assert!(rp.is_role_bound());

        let caveats = rp.caveats.unwrap().to_caveats();
        assert_eq!(caveats.fs_read, Scope::All);
        assert_eq!(caveats.fs_write, Scope::only(["src/".to_string()]));
        assert_eq!(caveats.exec, Scope::only(["cargo".to_string()]));
        assert_eq!(caveats.net, Scope::none());
        assert_eq!(caveats.max_calls, CountBound::AtMost(40));
    }

    #[test]
    fn parses_psyche_fields() {
        // A persona (Bob the researcher) that names its backend and pegs the
        // psyche dials — the shape the `obsessive`/`bob` shipped personas use.
        let text = "\
+++
role = \"researcher\"
backend = \"sol\"
cognition = \"meticulous\"
initiative = \"eager\"
crew = true
+++

# Bob

You are Bob, a researcher.
";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.role.as_deref(), Some("researcher"));
        assert_eq!(rp.backend.as_deref(), Some("sol"));
        assert_eq!(rp.cognition, Some(Cognition::Meticulous));
        assert_eq!(rp.initiative, Some(Initiative::Eager));
        assert_eq!(rp.crew, Some(true));
        assert!(rp.is_role_bound());
        // The dial maps onto the OpenAI reasoning-effort wire value.
        assert_eq!(rp.cognition.unwrap().reasoning_effort(), "high");
        assert_eq!(rp.cognition.unwrap().label(), "meticulous");
    }

    #[test]
    fn cognition_alone_is_role_bound() {
        // Even a single psyche dial promotes a persona past prompt-only.
        let text = "+++\ncognition = \"rational\"\n+++\n\n# Thinker\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.cognition, Some(Cognition::Rational));
        assert!(rp.is_role_bound());
        assert_eq!(Cognition::default(), Cognition::Thoughtful);
    }

    /// Slice 1a rename: an old label is refused with its replacement named,
    /// never silently accepted; persona files reach the strict parser only
    /// after `psyche_import` has rewritten them.
    #[test]
    fn old_cognition_labels_are_refused_with_the_rename() {
        for (old, new) in crate::psyche_import::LEGACY_COGNITION {
            let err = old.parse::<Cognition>().unwrap_err();
            assert_eq!(err, format!("cognition '{old}' was renamed to '{new}'"));
            let text = format!("+++\ncognition = \"{old}\"\n+++\nBody\n");
            assert!(RoleProfile::parse(&text).is_err(), "{old} must stay strict");
        }
        assert_eq!("default".parse::<Cognition>(), Ok(Cognition::Thoughtful));
        assert_eq!(
            " Meticulous ".parse::<Cognition>(),
            Ok(Cognition::Meticulous)
        );
        assert_eq!(
            "telepathic".parse::<Cognition>().unwrap_err(),
            "unknown cognition 'telepathic' (zen|rational|thoughtful|meticulous)"
        );
    }

    const PERSONALITY_TRAITS: [&str; 5] = [
        "agreeableness",
        "extraversion",
        "warmth",
        "approachability",
        "prosocial_behavior",
    ];

    /// These integers are operator-selected style levels, not psychological
    /// measurements. Every inclusive 0..=100 value must be accepted, and even
    /// one explicitly selected trait makes the persona more than prompt-only.
    #[test]
    fn personality_only_front_matter_is_role_bound_for_each_valid_level() {
        for (trait_name, trait_kind) in PERSONALITY_TRAITS.into_iter().zip(PersonalityTrait::ALL) {
            assert_eq!(trait_kind.key(), trait_name);
            for level in 0..=100 {
                let text = format!(
                    "+++\n[personality]\n{trait_name} = {level}\n+++\n\n# Friendly coder\n"
                );
                let rp = RoleProfile::parse(&text).unwrap();
                assert!(
                    rp.is_role_bound(),
                    "{trait_name} = {level} must bind the role"
                );
                assert_eq!(rp.prompt, "# Friendly coder");
                let traits = rp.personality.unwrap();
                assert_eq!(traits.get(trait_kind).unwrap().get(), level);
                for other in PersonalityTrait::ALL {
                    if other != trait_kind {
                        assert_eq!(traits.get(other), None);
                    }
                }
            }
        }
    }

    #[test]
    fn personality_traits_refuse_out_of_range_and_non_integer_levels() {
        for trait_name in PERSONALITY_TRAITS {
            for invalid in [
                "-1", "101", "255", "256", "1.0", "nan", "inf", "\"50\"", "true", "[]", "{}",
            ] {
                let text = format!(
                    "+++\n[personality]\n{trait_name} = {invalid}\n+++\n\n# Friendly coder\n"
                );
                assert!(
                    RoleProfile::parse(&text).is_err(),
                    "{trait_name} = {invalid} must be refused, not ignored or clamped"
                );
            }
        }
    }

    #[test]
    fn personality_metadata_preserves_the_complete_restrictive_role() {
        let metadata = r#"role = "reviewer"
tools = ["read_file"]
skills = ["review-guidance"]
altitude = "coach"
model = "local-test-model"
backend = "local"
tier = "REVIEW"
cognition = "rational"
initiative = "patient"
crew = false

[caveats]
fs_read = ["workspace"]
fs_write = "none"
exec = "none"
net = "none"
max_calls = 7
"#;
        let body = "# Reviewer\n\nExplain findings without editing files.";
        let baseline = RoleProfile::parse(&format!("+++\n{metadata}+++\n\n{body}")).unwrap();
        let personality = "[personality]\nagreeableness = 0\nextraversion = 25\nwarmth = 50\napproachability = 75\nprosocial_behavior = 100\n";
        let styled =
            RoleProfile::parse(&format!("+++\n{metadata}\n{personality}+++\n\n{body}")).unwrap();

        assert!(styled.is_role_bound());
        assert_eq!(styled.prompt, baseline.prompt);
        assert_eq!(styled.role, baseline.role);
        assert_eq!(styled.tools, baseline.tools);
        assert_eq!(styled.skills, baseline.skills);
        assert_eq!(styled.altitude, baseline.altitude);
        assert_eq!(styled.model, baseline.model);
        assert_eq!(styled.backend, baseline.backend);
        assert_eq!(styled.tier, baseline.tier);
        assert_eq!(styled.cognition, baseline.cognition);
        assert_eq!(styled.initiative, baseline.initiative);
        assert_eq!(styled.crew, baseline.crew);
        assert_eq!(styled.caveats, baseline.caveats);
        let mut escaped = styled.clone();
        escaped.backend = Some("local \"quoted\" \\ route".to_string());
        escaped
            .prompt
            .push_str("\n\nKeep `code`, \"quotes\", and \\ paths.");
        let encoded = escaped.to_markdown().unwrap();
        assert_eq!(RoleProfile::parse(&encoded).unwrap(), escaped);
        let caveats = styled.caveats.unwrap().to_caveats();
        assert_eq!(caveats.fs_read, Scope::only(["workspace".to_string()]));
        assert_eq!(caveats.fs_write, Scope::none());
        assert_eq!(caveats.exec, Scope::none());
        assert_eq!(caveats.net, Scope::none());
        assert_eq!(caveats.max_calls, CountBound::AtMost(7));
    }

    #[test]
    fn to_markdown_preserves_prompt_only_semantics_even_with_a_metadata_like_body() {
        for body in ["# Friendly coder", "+++\nThis is body text, not TOML.\n+++"] {
            let profile = RoleProfile {
                prompt: body.to_string(),
                ..RoleProfile::default()
            };
            let decoded = RoleProfile::parse(&profile.to_markdown().unwrap()).unwrap();
            assert_eq!(decoded, profile);
            assert!(!decoded.is_role_bound());
        }
    }

    #[test]
    fn to_markdown_refuses_metadata_that_serializes_as_an_envelope_fence() {
        // Escaped newlines make this a valid input profile, but the TOML writer
        // emits a multiline value containing an actual closing-fence line.
        let profile =
            RoleProfile::parse("+++\nrole = \"reviewer\\n+++\\nextra\"\n+++\n\n# Reviewer\n")
                .expect("escaped metadata is a valid source profile");
        assert_eq!(profile.role.as_deref(), Some("reviewer\n+++\nextra"));
        let error = profile
            .to_markdown()
            .expect_err("refuse an unreadable envelope before a saver can overwrite its source");
        assert!(matches!(
            error.downcast_ref::<crate::markup::EnvelopeError>(),
            Some(crate::markup::EnvelopeError::FrontMatterContainsFence)
        ));
    }

    #[test]
    fn to_markdown_refuses_a_style_edit_that_outgrows_a_valid_envelope() {
        let limit = crate::markup::MAX_ENVELOPE_BYTES;
        let metadata = format!("role = \"{}\"\n", "r".repeat(limit - "role = \"\"\n".len()));
        assert_eq!(metadata.len(), limit);
        let mut profile = RoleProfile::parse(&format!("+++\n{metadata}+++\n\n# Reviewer\n"))
            .expect("the source metadata fits exactly within the envelope limit");
        let unchanged = profile
            .to_markdown()
            .expect("unchanged metadata still fits");
        assert_eq!(RoleProfile::parse(&unchanged).unwrap(), profile);
        profile.personality = Some(PersonalityTraits {
            warmth: Some(PersonalityLevel::try_from(0).unwrap()),
            ..Default::default()
        });
        let error = profile
            .to_markdown()
            .expect_err("a valid style edit must not save an oversized envelope");
        assert!(matches!(
            error.downcast_ref::<crate::markup::EnvelopeError>(),
            Some(crate::markup::EnvelopeError::Oversized { bytes }) if *bytes > limit
        ));
    }

    #[test]
    fn personality_accessors_preserve_independent_values_and_content_identity() {
        use content_addressable::ContentAddressable;

        let mut traits = PersonalityTraits::default();
        let initial_id = traits.content_id().unwrap();
        for (index, kind) in PersonalityTrait::ALL.into_iter().enumerate() {
            traits.set(
                kind,
                Some(PersonalityLevel::try_from((index * 25) as u8).unwrap()),
            );
        }
        for (index, kind) in PersonalityTrait::ALL.into_iter().enumerate() {
            assert_eq!(traits.get(kind).unwrap().get(), (index * 25) as u8);
        }
        assert_ne!(traits.content_id().unwrap(), initial_id);
        let decoded: PersonalityTraits =
            toml::from_str(&toml::to_string(&traits).unwrap()).unwrap();
        assert_eq!(decoded.content_id().unwrap(), traits.content_id().unwrap());
        for kind in PersonalityTrait::ALL {
            traits.set(kind, None);
        }
        assert_eq!(traits.content_id().unwrap(), initial_id);
        assert!(PersonalityLevel::try_from(101).is_err());
        assert!(toml::from_str::<PersonalityTraits>("warmht = 50").is_err());
    }

    #[test]
    fn personality_prompt_block_omits_unspecified_traits_and_keeps_style_guardrails() {
        for kind in PersonalityTrait::ALL {
            for level in [0, 50, 100] {
                let mut traits = PersonalityTraits::default();
                traits.set(kind, Some(PersonalityLevel::try_from(level).unwrap()));
                let block = traits.prompt_block();
                assert!(block.starts_with("## Communication style\n"), "{block}");
                assert!(block.contains("Operator-selected communication-style levels (0-100)"));
                let line = format!("- {}: {level}/100", kind.label());
                assert_eq!(
                    block.lines().filter(|candidate| *candidate == line).count(),
                    1
                );
                for other in PersonalityTrait::ALL {
                    if other != kind {
                        assert!(!block.contains(&format!("- {}:", other.label())), "{block}");
                    }
                }
                let meaning = match kind {
                    PersonalityTrait::Agreeableness => [
                        "candid/challenging",
                        "collaborative framing",
                        "without agreeing with incorrect claims",
                    ],
                    PersonalityTrait::Extraversion => {
                        ["reserved", "conversational", "without unsolicited actions"]
                    }
                    PersonalityTrait::Warmth => ["neutral", "reassuring", "Lower"],
                    PersonalityTrait::Approachability => ["formal", "accessible", "Lower"],
                    PersonalityTrait::ProsocialBehavior => [
                        "task-focused",
                        "affected people/helpfulness",
                        "no unauthorized acts",
                    ],
                };
                for fragment in meaning {
                    assert!(
                        block.contains(fragment),
                        "{} meaning missing: {block}",
                        kind.key()
                    );
                }
                assert!(block
                    .contains("communication-style preferences, not psychological measurements"));
                assert!(block.contains("do not change tool authority, tenacity, or honesty"));
                assert!(block.contains("Remain civil and respectful even at low levels"));
            }
        }
    }

    #[test]
    fn personality_prompt_block_is_empty_for_inherited_style_and_has_one_canonical_order() {
        let mut traits = PersonalityTraits::default();
        assert!(traits.prompt_block().is_empty());
        for kind in PersonalityTrait::ALL.into_iter().rev() {
            traits.set(kind, Some(PersonalityLevel::try_from(25).unwrap()));
        }
        let block = traits.prompt_block();
        let lines: Vec<_> = block
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        let expected: Vec<_> = PersonalityTrait::ALL
            .into_iter()
            .map(|kind| format!("- {}: 25/100", kind.label()))
            .collect();
        assert_eq!(lines, expected);
        assert_eq!(block.matches("## Communication style").count(), 1);
        for kind in PersonalityTrait::ALL {
            traits.set(kind, None);
        }
        assert!(traits.prompt_block().is_empty());
    }

    #[test]
    fn personality_resolve_inherits_each_axis_but_explicit_zero_overrides_it() {
        for kind in PersonalityTrait::ALL {
            for level in [0, 100] {
                let mut inherited = PersonalityTraits::default();
                inherited.set(kind, Some(PersonalityLevel::try_from(level).unwrap()));
                assert_eq!(
                    PersonalityTraits::default().resolve(Some(inherited)),
                    inherited
                );
                let mut explicit = PersonalityTraits::default();
                explicit.set(kind, Some(PersonalityLevel::try_from(0).unwrap()));
                assert_eq!(explicit.resolve(Some(inherited)), explicit);
                assert_eq!(explicit.resolve(None), explicit);
            }
        }
        assert_eq!(
            PersonalityTraits::default().resolve(None),
            PersonalityTraits::default()
        );
    }

    #[test]
    fn personality_resolve_combines_mixed_overrides_without_changing_unspecified_axes() {
        let level = |value| Some(PersonalityLevel::try_from(value).unwrap());
        let inherited = PersonalityTraits {
            agreeableness: level(100),
            extraversion: level(0),
            warmth: level(25),
            approachability: level(75),
            prosocial_behavior: None,
        };
        let overrides = PersonalityTraits {
            agreeableness: level(0),
            extraversion: None,
            warmth: level(100),
            approachability: None,
            prosocial_behavior: level(50),
        };
        let expected = PersonalityTraits {
            agreeableness: level(0),
            extraversion: level(0),
            warmth: level(100),
            approachability: level(75),
            prosocial_behavior: level(50),
        };
        assert_eq!(overrides.resolve(Some(inherited)), expected);
        assert_eq!(overrides.resolve(None), overrides);
        assert_eq!(
            overrides.resolve(Some(PersonalityTraits::default())),
            overrides
        );
    }

    #[test]
    fn sparse_caveats_default_to_top() {
        // Only fs_write named; every other axis defaults to unrestricted.
        let text = "\
+++
role = \"reviewer\"
[caveats]
fs_write = \"none\"
+++

# Reviewer
Grade diffs.
";
        let rp = RoleProfile::parse(text).unwrap();
        let c = rp.caveats.unwrap().to_caveats();
        assert_eq!(c.fs_read, Scope::All);
        assert_eq!(c.fs_write, Scope::none());
        assert_eq!(c.exec, Scope::All);
        assert_eq!(c.net, Scope::All);
        assert_eq!(c.max_calls, CountBound::Unlimited);
    }

    #[test]
    fn malformed_front_matter_errors_clearly() {
        let text = "\
+++
role = \"oops
+++

body
";
        let err = RoleProfile::parse(text).unwrap_err().to_string();
        assert!(
            err.contains("front-matter"),
            "error should mention front-matter, got: {err}"
        );
    }

    #[test]
    fn unclosed_front_matter_errors() {
        let text = "+++\nrole = \"worker\"\n\n# body never fenced\n";
        let err = RoleProfile::parse(text).unwrap_err().to_string();
        assert!(
            err.contains("never closed"),
            "error should mention unclosed fence, got: {err}"
        );
    }

    #[test]
    fn empty_front_matter_block_is_valid() {
        // A `+++ +++` with nothing between is valid empty TOML → all None.
        let text = "+++\n+++\n\n# Just a prompt\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "# Just a prompt");
        assert!(!rp.is_role_bound());
    }

    /// FR-5 (#999): `altitude` parses (with `advise` as a synonym for `coach`),
    /// defaults to `None` when absent, and counts as role-binding front-matter.
    #[test]
    fn parses_altitude_with_advise_synonym() {
        let coach = RoleProfile::parse("+++\naltitude = \"coach\"\n+++\n\nx\n").unwrap();
        assert_eq!(coach.altitude, Some(Altitude::Coach));
        assert!(coach.is_role_bound(), "altitude alone binds the role");
        assert_eq!(
            RoleProfile::parse("+++\naltitude = \"advise\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            Some(Altitude::Coach),
            "`advise` is a synonym for coach"
        );
        assert_eq!(
            RoleProfile::parse("+++\naltitude = \"doer\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            Some(Altitude::Doer)
        );
        assert_eq!(
            RoleProfile::parse("+++\nrole = \"w\"\n+++\n\nx\n")
                .unwrap()
                .altitude,
            None,
            "absent altitude is None (doer applies downstream)"
        );
    }

    /// FR-4 (#1041): `skills` parses as a list of skill names, defaults to
    /// `None` when absent, and counts as role-binding front-matter.
    #[test]
    fn parses_skills_field() {
        let rp = RoleProfile::parse(
            "+++\nskills = [\"gila-personal-assistant\", \"other-skill\"]\n+++\n\nx\n",
        )
        .unwrap();
        assert_eq!(
            rp.skills,
            Some(vec![
                "gila-personal-assistant".to_string(),
                "other-skill".to_string(),
            ])
        );
        assert!(rp.is_role_bound(), "skills alone binds the role");
        assert_eq!(
            RoleProfile::parse("+++\nrole = \"w\"\n+++\n\nx\n")
                .unwrap()
                .skills,
            None,
            "absent skills is None (no bound skills)"
        );
    }

    #[test]
    fn leading_plus_text_is_not_a_fence() {
        // `+++foo` on the first line is body text, not a fence opener.
        let text = "+++foo bar\nstill body\n";
        let rp = RoleProfile::parse(text).unwrap();
        assert_eq!(rp.prompt, "+++foo bar\nstill body");
        assert!(!rp.is_role_bound());
    }

    #[test]
    fn caveat_profile_summary_is_one_line_per_axis() {
        let text = "\
+++
role = \"worker\"
[caveats]
fs_read = \"all\"
fs_write = \"none\"
exec = [\"cargo\"]
max_calls = 40
+++

# Worker
body
";
        let rp = RoleProfile::parse(text).unwrap();
        let summary = rp.caveats.unwrap().summary();
        assert_eq!(
            summary,
            "fs_read=all fs_write=none exec=[cargo] net=all max_calls=40"
        );
    }

    // --- #1021 PR 5.2: headless `RoleProfile::load_from_dir` -------------

    #[test]
    fn load_from_dir_parses_an_existing_persona() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("personal-assistant.md"),
            "+++\nrole = \"personal-assistant\"\ntools = [\"a\"]\n+++\n\n# PA\nbody\n",
        )
        .unwrap();
        let rp = RoleProfile::load_from_dir("personal-assistant", tmp.path(), &mut |_| {}).unwrap();
        assert_eq!(rp.role.as_deref(), Some("personal-assistant"));
        assert_eq!(rp.prompt, "# PA\nbody");
    }

    #[test]
    fn load_from_dir_errors_loudly_on_a_missing_file_no_seeding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = RoleProfile::load_from_dir("nope", tmp.path(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
        assert!(
            !tmp.path().join("nope.md").exists(),
            "a missing persona must NOT be silently seeded (headless callers fail loudly instead)"
        );
    }

    #[test]
    fn load_from_dir_rejects_invalid_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = RoleProfile::load_from_dir("bad name!", tmp.path(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("letters, numbers"), "got: {err}");
    }

    #[test]
    fn load_from_dir_rejects_an_empty_persona_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("blank.md"), "   \n").unwrap();
        let err = RoleProfile::load_from_dir("blank", tmp.path(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("is empty"), "got: {err}");
    }

    /// Slice 1a rename: the headless `--persona` path migrates an old
    /// cognition label instead of failing the strict parse. Grounds the
    /// mocked `psyche_import` seam test: this real read reaches the importer.
    #[test]
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    fn load_from_dir_migrates_an_old_cognition_label() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("deep.md");
        std::fs::write(&path, "+++\ncognition = \"contemplating\"\n+++\n# Deep\n").unwrap();
        let rp = RoleProfile::load_from_dir("deep", tmp.path(), &mut |_| {}).unwrap();
        assert_eq!(rp.cognition, Some(Cognition::Meticulous));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "+++\ncognition = \"meticulous\"\n+++\n# Deep\n"
        );
    }

    #[test]
    fn load_from_dir_lowercases_the_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("coach.md"), "# Coach\nbody\n").unwrap();
        let rp = RoleProfile::load_from_dir("  CoAcH  ", tmp.path(), &mut |_| {}).unwrap();
        assert_eq!(rp.prompt, "# Coach\nbody");
    }
}
