//! Resolved model identity — **display names are labels, never evidence**.
//!
//! # The law
//!
//! No authoritative execution behavior may be selected by matching a display
//! model ID. Structured metadata or an explicit operator declaration may
//! determine identity; otherwise the family is **Unknown** and
//! family-specific policy does not apply. String recognition may only produce
//! a visible, non-authoritative [`FamilySuggestion`] an operator confirms.
//!
//! An operator serves any artifact under any alias — `ollama create my-helper`
//! over a Qwen3 GGUF yields an id that says nothing, and a fine-tune published
//! as `qwen-ish` may share no lineage. Reading the label is wrong in **both**
//! directions, and both are silent.
//!
//! # What this is, and what it is not
//!
//! [`ModelCard`](crate::model_card::ModelCard) is the **declaration format** —
//! what an operator writes in TOML. This is the **resolution result**: which
//! family, if any, is authoritative enough to key policy with, and on what
//! evidence. Card in, resolution out; the shared `family: Option<String>` is
//! that pipeline's input and output, not two implementations of one idea.
//!
//! [`TuneSource`](crate::tuning::TuneSource) records where a tuning *value*
//! came from (including `Empirical` — measured, which has no identity
//! analogue) and is serialized in community profile files at format version
//! "1". [`FamilyEvidence`] records how *identity* was established. Adjacent
//! axes, deliberately not merged: unifying them would break a published file
//! format to express a resemblance rather than a shared meaning.
//!
//! # The one invariant
//!
//! **A family is present exactly when its evidence is authoritative.** The
//! constructors enforce it, so "a family from nowhere" and "authoritative
//! evidence for nothing" are unrepresentable rather than merely discouraged —
//! and one accessor, [`ResolvedModel::family_policy_key`], is the only way to
//! reach a family-keyed table.

use serde::{Deserialize, Serialize};

/// How a model's family was established, strongest first. Declaration
/// outranks discovery: a declaration is the operator exercising authority
/// they hold, discovery is an inference we made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyEvidence {
    /// An explicit operator declaration — a card they wrote, a config
    /// override, or a confirmed suggestion.
    OperatorCard,
    /// An exact match against a resolved card or registry entry, keyed on the
    /// full model id (never a prefix or substring).
    ResolvedCard,
    /// Carried by the artifact — GGUF `general.architecture`, a local
    /// `config.json` `architectures` entry.
    ArtifactMetadata,
    /// Reported natively by the provider — Ollama `details.family` /
    /// `details.families` / `model_info`.
    ProviderMetadata,
    /// Nothing authoritative. The id is a label; the family is Unknown.
    Unresolved,
}

impl FamilyEvidence {
    /// Strictly stronger than `other`. Irreflexive, so an equally-sourced
    /// later candidate never silently displaces an earlier one.
    #[must_use]
    pub fn outranks(self, other: Self) -> bool {
        (self as u8) < (other as u8)
    }

    /// Whether this evidence may determine authoritative behavior.
    #[must_use]
    pub fn is_authoritative(self) -> bool {
        self != Self::Unresolved
    }
}

/// A model's resolved identity.
///
/// `id` is always present and is always a **label**. `architecture` and
/// `artifact` are descriptive metadata — they key no policy, so they carry no
/// precedence and are merged from whichever source knows them.
///
/// Deliberately no `variant` field: nothing consumes one yet, and a field
/// with no reader is a guess about the future that later code must honor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModel {
    id: String,
    family: Option<String>,
    family_evidence: FamilyEvidence,
    architecture: Option<String>,
    artifact: Option<String>,
}

impl ResolvedModel {
    /// A model known only by its label — the Unknown floor, and the correct
    /// result for any endpoint exposing only an id (a generic
    /// OpenAI-compatible `/v1/models`).
    ///
    /// Deliberately the easiest constructor to reach: the safe answer should
    /// not be the effortful one.
    #[must_use]
    pub fn unknown(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            family: None,
            family_evidence: FamilyEvidence::Unresolved,
            architecture: None,
            artifact: None,
        }
    }

    /// A family established by `evidence`.
    ///
    /// A `None` or blank family downgrades to [`FamilyEvidence::Unresolved`]
    /// whatever the caller claimed — evidence is only as good as what it
    /// carried, and a provider that returned an id and no family resolved
    /// nothing however it was asked. This is what makes the module's one
    /// invariant hold by construction.
    #[must_use]
    pub fn declared(id: impl Into<String>, family: Option<&str>, evidence: FamilyEvidence) -> Self {
        let family = normalize(family);
        Self {
            id: id.into(),
            family_evidence: if family.is_some() {
                evidence
            } else {
                FamilyEvidence::Unresolved
            },
            family,
            architecture: None,
            artifact: None,
        }
    }

    /// Attach descriptive metadata. Not evidence for a family: an
    /// architecture describes the thing, it does not name its lineage, and
    /// conflating the two is how `qwen3moe` would come to imply a policy key.
    #[must_use]
    pub fn with_metadata(mut self, architecture: Option<&str>, artifact: Option<&str>) -> Self {
        self.architecture = normalize(architecture).or(self.architecture);
        self.artifact = normalize(artifact).or(self.artifact);
        self
    }

    /// Resolve one identity for `id` from every candidate.
    ///
    /// The family comes from the strongest candidate that has one; the
    /// descriptive metadata is **merged**, not discarded. That merge is the
    /// point: an operator card declares the family while the provider knows
    /// the architecture and digest, and taking the card wholesale would throw
    /// the provider's facts away — a silent loss that the previous
    /// wholesale-winner version shipped and its own test failed to notice.
    ///
    /// `id` is a parameter because it is always known: resolution asks what
    /// evidence exists ABOUT a model, never which model this is. With no
    /// candidates the answer is `unknown(id)` — never a fabricated identity
    /// with an empty id.
    #[must_use]
    pub fn resolve(id: impl Into<String>, candidates: impl IntoIterator<Item = Self>) -> Self {
        let mut out = Self::unknown(id);
        for c in candidates {
            if c.family_evidence.outranks(out.family_evidence) {
                out.family = c.family;
                out.family_evidence = c.family_evidence;
            }
            out.architecture = out.architecture.or(c.architecture);
            out.artifact = out.artifact.or(c.artifact);
        }
        out
    }

    /// The display id. **A label** — never branch authoritative behavior on it.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The key to index family-specific policy with, or `None` when no family
    /// policy may apply.
    ///
    /// The ONLY family accessor. A plain `family()` beside it would invite
    /// exactly the mistake this module exists to prevent — read it, find
    /// `None`, and reach for a default.
    #[must_use]
    pub fn family_policy_key(&self) -> Option<&str> {
        self.family.as_deref()
    }

    /// How the family was established.
    #[must_use]
    pub fn family_evidence(&self) -> FamilyEvidence {
        self.family_evidence
    }

    /// The declared architecture, when structured metadata carried one.
    #[must_use]
    pub fn architecture(&self) -> Option<&str> {
        self.architecture.as_deref()
    }

    /// The artifact identity (digest) — the only field naming the actual
    /// bytes served.
    #[must_use]
    pub fn artifact(&self) -> Option<&str> {
        self.artifact.as_deref()
    }

    /// Whether this model authoritatively belongs to `family`
    /// (case-insensitive on the RESOLVED value — never on the id).
    #[must_use]
    pub fn is_family(&self, family: &str) -> bool {
        let want = family.trim();
        !want.is_empty()
            && self
                .family_policy_key()
                .is_some_and(|f| f.eq_ignore_ascii_case(want))
    }
}

/// A **non-authoritative** family hint derived from a model's name.
///
/// A distinct type from [`ResolvedModel`] on purpose: there is no path from a
/// substring to authority except [`Self::confirm`], which is an operator act.
/// A hint may be shown; it may not be acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilySuggestion {
    /// The family the name resembles.
    pub family: String,
    /// The id the match was found in, so the guess is inspectable.
    pub matched_in: String,
}

impl FamilySuggestion {
    /// Always `false` — stated in code at every use site rather than left to
    /// convention.
    #[must_use]
    pub fn is_authoritative(&self) -> bool {
        false
    }

    /// The operator confirms the hint. The result is
    /// [`FamilyEvidence::OperatorCard`] because **the confirmation is the
    /// evidence**; the substring only prompted a human to declare something.
    #[must_use]
    pub fn confirm(&self, id: impl Into<String>) -> ResolvedModel {
        ResolvedModel::declared(id, Some(&self.family), FamilyEvidence::OperatorCard)
    }
}

/// Offer a family hint by matching a known family key inside a model name.
///
/// The match set is the caller's own configured families — data, not a
/// hardcoded lineage table. `None` when nothing matches: absence is reported
/// as absence, never as a default.
#[must_use]
pub fn suggest_family_from_name(id: &str, known_families: &[String]) -> Option<FamilySuggestion> {
    let lower = id.to_ascii_lowercase();
    known_families
        .iter()
        .find(|k| {
            let k = k.trim().to_ascii_lowercase();
            !k.is_empty() && lower.contains(&k)
        })
        .map(|k| FamilySuggestion {
            family: k.trim().to_string(),
            matched_in: id.to_string(),
        })
}

/// Trim and drop empties — an empty declaration is an absent one, not a
/// family named "".
fn normalize(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
