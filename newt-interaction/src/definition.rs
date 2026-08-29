//! The immutable, semantic definition of one interaction.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{ControlId, DefinitionId, OptionId, Revision};

/// The versioned type tag every definition carries as its first field.
///
/// Bound into identity, so bumping it re-addresses every definition — the
/// property `newt_core::agentic::content_spill`'s `SPILL_SCHEMA_V1` already
/// relies on (`content_spill.rs:45-47`).
pub const DEFINITION_SCHEMA_V1: &str = "newt.interaction.definition/v1";

/// What kind of interaction this is. The kind is semantic, never a renderer
/// choice: `modal` is a view decision (ADR C1), not a kind.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InteractionKind {
    /// Pick one of a fixed, displayed set — today's `Question<A>`.
    ///
    /// Not a two-option yes/no: that is [`InteractionKind::Confirm`].
    Choice,
    /// Free text.
    Prompt,
    // WHY THIS EXISTS BESIDE `Choice`, and why the boundary is enforced
    // (#1912). A `//` comment, not a doc comment, DELIBERATELY: these docs are
    // rendered into `schema/definition.schema.json` by schemars and read by
    // non-Rust consumers, so the published description should state the
    // contract and not this repo's incident history.
    //
    // Until the guard existed, the tree declared the same decision-shaped
    // interaction under both kinds — `agentic::tools`'s mutation confirm and
    // `interaction_adapter` and the permission builder as `Choice`,
    // `interaction_form::confirm` as `Confirm`. C0c was the first slice to try
    // to vary behaviour by the kind and had to go unconditional instead.
    //
    // `Confirm` is NOT redundant with `Choice`: a lone `ControlKind::Toggle`
    // is a yes/no, and `Choice` means "pick one of a fixed, DISPLAYED set",
    // which a toggle has none of. Two shapes, one intent, and the kind is what
    // unifies them. See `InteractionDefinition::is_decision_shaped` for which
    // direction is enforceable and why the other is not.
    /// A yes/no decision — proceed or not.
    ///
    /// Carried either by a single toggle, or by one choice control offering
    /// exactly two options where one grants and the other refuses or backs
    /// out. The distinction from [`InteractionKind::Choice`] is the ROLES the
    /// options carry, never how many there are.
    Confirm,
    /// A multi-field form.
    Form,
    /// A notice that carries no controls and expects no response.
    Notice,
}

/// What a control MEANS, independent of how any surface draws it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SemanticRole {
    /// Grants the thing being asked about.
    Allow,
    /// Refuses it. The fail-closed default for an absent answer.
    Deny,
    /// Backs out without deciding.
    Cancel,
    /// Ends the session.
    Exit,
    /// Supplies a value rather than a decision.
    Value,
}

/// Whether something is REQUIRED to present or answer an interaction
/// faithfully, or merely preferred.
///
/// This is the axis ADR law 5 turns on: *unknown REQUIRED behavior fails
/// closed; unknown OPTIONAL behavior degrades visibly.* Without it a
/// consumer that cannot satisfy a demand has only one move for both cases,
/// and whichever it picks is wrong half the time — silently dropping a
/// mandatory secret field, or refusing a document that merely wanted a
/// diagram.
///
/// One vocabulary, deliberately: controls and surface features both use
/// it, so a wire consumer learns the distinction once. On a control it
/// means "the response must answer this FIELD" — which is only coherent
/// because a choice is one field with many options, not many fields.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Requirement {
    /// Must be satisfiable. If it is not, the interaction is
    /// `Unsupported` and no answer may be guessed.
    Required,
    /// Preferred. If it is not satisfiable, the interaction proceeds and
    /// the shortfall is reported visibly.
    Optional,
}

/// One mutually-exclusive option of a choice control.
///
/// The option carries the SemanticRole, not the control: a permission
/// question has no single meaning — each of its options does. `allow once`
/// means Allow, `deny (default)` means Deny, and they are options of one
/// question rather than two questions.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    /// Stable within the control; the definition commits to it.
    pub id: OptionId,
    /// What picking this option means.
    pub role: SemanticRole,
    /// Human-readable label. Labels never confer authority (ADR law 2).
    pub label: String,
    /// The accelerator a keyboard surface offers for this option — `a`,
    /// `d`. Presentation, but NOT only presentation: the terminal
    /// authorizes on it, so it is part of what the definition means and
    /// cannot live in the view.
    ///
    /// Omitted when empty, so an option that offers no accelerator is
    /// byte-identical to one written before this field existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Hidden inputs that also select this option — `n`/`N` for a deny.
    /// Never rendered, and never able to shadow another option's
    /// canonical id or key: matching is canonical-first, and ambiguity
    /// denies.
    ///
    /// Omitted when empty, for the same reason as `key`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// How a control accepts input.
///
/// `Choice` carries its options inline because they belong to it: one
/// field, N mutually-exclusive answers. Modelling each option as its own
/// control would make [`Requirement`] incoherent — under "the response
/// must include this control", answering an allow would require also
/// answering the deny.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlKind {
    /// Pick exactly one of the options offered.
    Choice {
        /// The options, in presentation order.
        options: Vec<ChoiceOption>,
    },
    /// Free text.
    Text,
    /// A boolean.
    Toggle,
    /// A secret: never persisted in markup or logs (ADR D1).
    Secret,
}

/// One control: a stable id, what it means, and how it takes input.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// Stable within the definition; the definition commits to it.
    pub id: ControlId,
    /// How it accepts input, and — for a choice — what it offers.
    pub kind: ControlKind,
    /// Human-readable label. Labels never confer authority (ADR law 2).
    pub label: String,
    /// Whether a response must include this control.
    pub requirement: Requirement,
}

/// A capability a surface must have to present some part of an
/// interaction: accepting secret input without echoing it, rendering a
/// diagram, showing more than one control at once.
///
/// A named string rather than a closed enum, on purpose. This is a WIRE
/// vocabulary: a v2 document may demand a feature this build has never
/// heard of, and a closed enum could not even represent it — the name
/// would be lost at parse time and law 5 would have nothing left to act
/// on. Carrying the name verbatim is what lets an unknown demand be
/// refused (when required) or reported (when optional) instead of
/// silently vanishing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
// See the note on `ControlId`: deserialization goes through the
// constructor. An UNRECOGNIZED feature name is still valid — that is the
// forward-compatibility case — but an EMPTY one is not.
#[serde(into = "String", try_from = "String")]
pub struct SurfaceFeature(String);

#[cfg(feature = "schema")]
crate::string_scalar_schema!(SurfaceFeature, None::<&str>);

impl From<SurfaceFeature> for String {
    fn from(value: SurfaceFeature) -> Self {
        value.0
    }
}

impl TryFrom<String> for SurfaceFeature {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl SurfaceFeature {
    /// The surface accepts secret input without echoing or persisting it.
    pub const SECRET_INPUT: &'static str = "secret-input";
    /// The surface renders a diagram extension (epic slice E0).
    pub const DIAGRAMS: &'static str = "diagrams";
    /// The surface can present more than one control at once.
    pub const MULTI_CONTROL: &'static str = "multi-control";

    /// Every feature name this build understands.
    pub const KNOWN: &'static [&'static str] =
        &[Self::SECRET_INPUT, Self::DIAGRAMS, Self::MULTI_CONTROL];

    /// Adopt a feature name.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty. An UNRECOGNIZED name is
    /// not an error — that is the forward-compatibility case, and
    /// [`is_known`](Self::is_known) is how a consumer asks.
    pub fn new(name: impl Into<String>) -> Result<Self, ProtocolError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "surface feature",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(name))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this build understands the feature.
    #[must_use]
    pub fn is_known(&self) -> bool {
        Self::KNOWN.contains(&self.0.as_str())
    }
}

/// A definition's demand for one surface feature, and how hard a demand it
/// is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureDemand {
    /// Which capability.
    pub feature: SurfaceFeature,
    /// Required, or merely preferred.
    pub requirement: Requirement,
}

/// The immutable semantic model of one interaction.
///
/// Identity is a [`DefinitionId`] — a `ContentId` over the canonical
/// encoding of this whole record, which is also its **exact form digest**.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionDefinition {
    /// Versioned type tag; see [`DEFINITION_SCHEMA_V1`].
    pub schema: crate::tag::DefinitionTag,
    /// What kind of interaction this is.
    pub kind: InteractionKind,
    /// Which revision of this definition's authoring lineage this is.
    pub revision: Revision,
    /// The readable body — canonical Markdown, the fallback every surface
    /// can render (ADR law 1). Held as text; parsing belongs to
    /// `newt_core::markup`, not to the protocol layer.
    pub markdown: String,
    /// The controls, in presentation order.
    pub controls: Vec<Control>,
    /// What a surface must be able to do to present this faithfully, each
    /// demand marked required or optional.
    pub features: Vec<FeatureDemand>,
    /// A subordinate line beneath the body — a control hint, a danger
    /// warning. Readable content like `markdown`, kept separate because
    /// surfaces place it differently and the legacy prompt already
    /// distinguishes the two.
    ///
    /// Omitted when absent, so a definition without one is byte-identical
    /// to one written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Whether `controls` can ONLY be a binary decision — see
/// [`InteractionDefinition::is_decision_shaped`], which delegates here.
///
/// Free-standing so a caller that is still ASSEMBLING a definition can pick
/// the kind from the controls it has just built, without duplicating the rule.
/// `interaction_adapter` is that caller: it converts a legacy `Question` whose
/// action count it does not know in advance, and hardcoding a kind there is
/// what made the adapter emit decision-shaped definitions labelled `Choice`
/// (#1912). One rule, two callers, and the guard in
/// [`InteractionDefinition::new`] holds them to it.
#[must_use]
pub fn controls_are_decision_shaped(controls: &[Control]) -> bool {
    let [control] = controls else {
        return false;
    };
    let ControlKind::Choice { options } = &control.kind else {
        return false;
    };
    let [a, b] = options.as_slice() else {
        return false;
    };
    let decides = |r: SemanticRole| matches!(r, SemanticRole::Deny | SemanticRole::Cancel);
    (a.role == SemanticRole::Allow && decides(b.role))
        || (b.role == SemanticRole::Allow && decides(a.role))
}

impl InteractionDefinition {
    /// Whether this definition can ONLY be a binary decision: one
    /// [`ControlKind::Choice`] control offering exactly two options, one
    /// granting and the other refusing or backing out.
    ///
    /// Keys on [`SemanticRole`], never on the option count. `Allow` + `Deny`
    /// is a decision, `Allow` + `Cancel` is a decision that offers a way out,
    /// and `Value` + `Value` is a two-way pick and not a decision at all.
    ///
    /// **Deliberately narrower than "is a yes/no", and the gap is the point.**
    /// A lone [`ControlKind::Toggle`] is also a yes/no — and it is the case
    /// that settles #1912, because [`InteractionKind::Choice`] means "pick one
    /// of a fixed, DISPLAYED set" and a toggle displays no set, so `Choice`
    /// cannot describe it and `Confirm` is not redundant with it.
    ///
    /// But a lone toggle is ALSO a one-field form — "remember this? [ ]" — and
    /// nothing in the shape says which. So the toggle case is admitted by
    /// [`InteractionKind::Confirm`] and never *required* of it. This predicate
    /// reports only what a shape can prove, which is why the guard in
    /// [`InteractionDefinition::new`] enforces one direction and not both.
    #[must_use]
    pub fn is_decision_shaped(&self) -> bool {
        controls_are_decision_shaped(&self.controls)
    }

    /// Whether a lone [`ControlKind::Toggle`] carries this definition — the
    /// other shape [`InteractionKind::Confirm`] may take.
    #[must_use]
    fn is_lone_toggle(&self) -> bool {
        matches!(self.controls.as_slice(), [c] if matches!(c.kind, ControlKind::Toggle))
    }

    /// Build a definition carrying the current schema tag.
    #[must_use]
    pub fn new(kind: InteractionKind, markdown: impl Into<String>, controls: Vec<Control>) -> Self {
        let built = Self {
            schema: crate::tag::DefinitionTag,
            kind,
            revision: Revision::FIRST,
            markdown: markdown.into(),
            controls,
            features: Vec::new(),
            note: None,
        };
        // **THE GUARD (#1912).** A doc comment saying "use Confirm for yes/no"
        // is not a guard — that is exactly what the tree had, and it drifted
        // in the one direction nobody was watching. This fires in every debug
        // build, so a SECOND constructor cannot reintroduce the ambiguity
        // without a test going red the first time it runs.
        //
        // `debug_assert` rather than a `Result`: the pairing is a property of
        // the code that constructs the definition, fixable at the call site
        // and never at runtime. Making `new` fallible would push a
        // `.expect()` onto ~15 infallible call sites to catch a bug none of
        // them can have in production.
        // The direction #1912 is about: a decision-shaped definition declared
        // as anything else is the ambiguity itself.
        debug_assert!(
            !built.is_decision_shaped() || kind == InteractionKind::Confirm,
            "a binary decision — one Toggle, or one Choice control offering \
             two options where one grants and the other refuses — is \
             InteractionKind::Confirm. See that variant's doc. Got kind \
             {kind:?} for controls {:?}",
            built.controls
        );
        // And the converse, SCOPED TO DEFINITIONS THAT HAVE CONTROLS. The
        // unscoped form looked tidier and was wrong: it fired immediately on
        // control-less `Confirm` fixtures across four newt-interaction tests,
        // which are building a definition to exercise something else and are
        // not claiming a shape at all. Policing an empty definition's kind is
        // not this guard's business; policing a populated one that is NOT a
        // yes/no still is — a five-option `Confirm` is mislabelled too.
        debug_assert!(
            kind != InteractionKind::Confirm
                || built.controls.is_empty()
                || built.is_decision_shaped()
                || built.is_lone_toggle(),
            "InteractionKind::Confirm carries a binary decision (a Toggle, or \
             two options one of which grants) or no controls at all; got \
             controls {:?}",
            built.controls
        );
        built
    }

    /// This definition's identity, which is also its exact form digest.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure.
    pub fn definition_id(&self) -> Result<DefinitionId, ProtocolError> {
        Ok(DefinitionId::from_content_id(self.content_id()?))
    }

    /// The schema tag, as a string.
    ///
    /// Reading it needs no check: the tag is a TYPE that deserializes from
    /// exactly one value, so a record of this type cannot carry any other
    /// one. What used to be a runtime `ensure_known_schema` is now a thing
    /// the wire cannot express — and the published schema says so with a
    /// `const`, so a foreign implementor validating against the wrong
    /// record's schema fails instead of passing.
    #[must_use]
    pub fn schema_tag(&self) -> &'static str {
        self.schema.as_str()
    }
}

impl ContentAddressable for InteractionDefinition {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        // The canonical form covers the WHOLE record — schema tag, kind,
        // revision, markdown, controls, and surface features. A form over a
        // hand-picked subset would let two definitions that differ in a
        // skipped field share one id, and an "exact form digest" that is not
        // exact is worse than none.
        canonical::to_canonical_dagcbor(self)
    }
}
