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

/// How much of a point in time a [`ControlKind::Temporal`] control asks for.
///
/// HTML's `date`, `time`, `datetime-local`, `month` and `week` — five input
/// types that differ only in precision, so they are five configurations of one
/// kind rather than five kinds.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TemporalPrecision {
    /// A calendar date: `YYYY-MM-DD`.
    Date,
    /// A wall-clock time: `HH:MM` or `HH:MM:SS`.
    Time,
    /// A local date and time: `YYYY-MM-DDTHH:MM[:SS]`.
    DateTime,
    /// A calendar month: `YYYY-MM`.
    Month,
    /// An ISO week: `YYYY-Www`.
    Week,
}

impl TemporalPrecision {
    /// The shape an answer must take, for a hint and for a refusal message.
    #[must_use]
    pub fn pattern(self) -> &'static str {
        match self {
            Self::Date => "YYYY-MM-DD",
            Self::Time => "HH:MM[:SS]",
            Self::DateTime => "YYYY-MM-DDTHH:MM[:SS]",
            Self::Month => "YYYY-MM",
            Self::Week => "YYYY-Www",
        }
    }
}

/// What a [`ControlKind::Path`] control points at.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PathKind {
    /// A single file.
    File,
    /// A directory.
    Directory,
}

/// How a control accepts input.
///
/// `Choice` carries its options inline because they belong to it: one
/// field, N mutually-exclusive answers. Modelling each option as its own
/// control would make [`Requirement`] incoherent — under "the response
/// must include this control", answering an allow would require also
/// answering the deny.
///
/// # A kind is a MEANING; an affordance is not a kind
///
/// The line this vocabulary is drawn on, because the HTML input catalog it
/// mirrors does not draw it and copying that list variant-for-variant would
/// have produced twenty-two.
///
/// A kind earns its place when it changes what a well-formed ANSWER is —
/// `#c0ffee` is a color and not a date, and no surface may accept it as one.
/// It does not earn a place merely by suggesting a different widget: a slider
/// and a typed number accept the same answers, so [`Range`](Self::Range) is
/// separate from [`Number`](Self::Number) only because a bounded-and-stepped
/// value space IS different (its bounds are mandatory), not because one draws
/// as a slider.
///
/// What this excludes, deliberately:
///
/// - `button`, `submit`, `reset`, `image` — form ACTIONS. A definition's
///   actions are its [`SemanticRole`]s; a submit button that carried its own
///   meaning would be a second authority vocabulary.
/// - `hidden` — not a control at all. A value the operator cannot see is not
///   a thing they answered, and putting one here would let a surface claim an
///   answer nobody gave.
/// - `radio`, `checkbox`, `password`, `select` — already spelled
///   [`Choice`](Self::Choice), [`Toggle`](Self::Toggle),
///   [`Secret`](Self::Secret) and [`Choice`](Self::Choice) again. `<select>`
///   and a radio group are one meaning with two presentations.
///
/// # Every kind here is answerable by typing
///
/// None of these mint a [`SurfaceFeature`] demand (see
/// `downgrade::intrinsic_demands`), and that is deliberate rather than
/// unfinished. A date answered by typing `2026-09-01` is a satisfied date, so
/// a surface with no calendar widget has NOT failed to present the question —
/// it presented it plainly. Only [`Secret`](Self::Secret) demands a feature,
/// because it changes what may be DISCLOSED rather than how something is
/// drawn. Minting a demand per kind would make plain and headless surfaces
/// refuse questions they can answer perfectly well, which is law 5 pointed at
/// the wrong axis.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ControlKind {
    /// Pick exactly one of the options offered.
    Choice {
        /// The options, in presentation order.
        options: Vec<ChoiceOption>,
    },
    /// Free text.
    ///
    /// **Deliberately still a unit variant.** HTML spells five flavours of
    /// text — `email`, `url`, `tel`, `search`, plain — and carrying one here
    /// as `Text { format }` would re-encode the most common kind in the
    /// vocabulary: a unit variant is the dag-cbor string `"text"`, a struct
    /// variant is a map. That breaks `tests/data/interaction-vectors.json`,
    /// whose frozen canonical bytes are DECODED rather than rebuilt, and the
    /// external-consumer corpus answering them — the compatibility fixtures
    /// CLAUDE.md names as what a rewrite is written against.
    ///
    /// The flavours differ, in a terminal, only in what counts as valid. That
    /// is not worth re-addressing every definition ever written. If one earns
    /// its own kind later it can be ADDED as a new variant, which breaks
    /// nothing — the way `Number`, `Temporal`, `Color` and `Path` were.
    Text,
    /// A boolean.
    Toggle,
    /// A secret: never persisted in markup or logs (ADR D1).
    Secret,
    /// A whole number, optionally bounded and stepped.
    ///
    /// **Integers, and that is a decision.** A binary float cannot be
    /// canonically encoded without deciding what to do about `NaN` and `-0.0`,
    /// and this vocabulary is content-addressed — two encodings of one value
    /// would be two identities. Every number newt asks for today (a round cap,
    /// a port, a timeout, a percentage) is integral. A fractional control is a
    /// later, separate decision, and it will carry its digits as a decimal
    /// STRING rather than reopening this one.
    Number {
        /// Smallest acceptable value, inclusive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        /// Largest acceptable value, inclusive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
        /// The grid the value must sit on, counted from `min` (or from zero
        /// when unbounded below). `None` accepts any integer in range.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<i64>,
    },
    /// A number on a bounded, stepped scale — HTML's `range`.
    ///
    /// Distinct from [`Number`](Self::Number) because its bounds are
    /// MANDATORY: a slider without ends is not a slider, and a surface that
    /// draws one needs both to draw anything at all.
    Range {
        /// Smallest acceptable value, inclusive.
        min: i64,
        /// Largest acceptable value, inclusive.
        max: i64,
        /// The grid the value must sit on, counted from `min`.
        step: i64,
    },
    /// A point in time at the given precision.
    Temporal {
        /// How much of a moment is being asked for.
        precision: TemporalPrecision,
    },
    /// An sRGB color, written `#rrggbb`.
    Color,
    /// A filesystem path.
    ///
    /// The protocol says what is being NAMED, never whether it exists: an
    /// existence check is the host's business, is racy by nature, and would
    /// make a definition's validity depend on the machine reading it.
    Path {
        /// Whether the path names a file or a directory.
        kind: PathKind,
        /// Extensions or globs a surface may filter by — HTML's `accept`.
        /// Advisory: a surface that ignores it still collects a valid answer.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        accept: Vec<String>,
    },
}

impl ControlKind {
    /// Is `text` a well-formed answer to this control?
    ///
    /// `Err` carries what was EXPECTED, phrased for a human — "an integer in
    /// 1..=10000", "YYYY-MM-DD", "#rrggbb". The caller decides what a
    /// malformed answer means: the binding layer refuses it, an interactive
    /// surface can show it as you type. One implementation either way, which
    /// is the point — a panel that validated `Number` its own way would be a
    /// second opinion about what the definition means.
    ///
    /// [`Choice`](Self::Choice) always answers `Ok`: typed input for a choice
    /// is RESOLVED (canonical-first, aliases second, ambiguity denies) by
    /// `binding::resolve_typed`, and a second, weaker check here is exactly
    /// the duplicate that rule exists to prevent. [`Toggle`](Self::Toggle) and
    /// [`Secret`](Self::Secret) do not travel as text at all.
    ///
    /// # Errors
    ///
    /// The expectation this control's answers must meet, when `text` does not.
    pub fn check_text(&self, text: &str) -> Result<(), String> {
        let text = text.trim();
        match self {
            Self::Choice { .. } | Self::Toggle | Self::Secret => Ok(()),
            // Free text: anything is well-formed. A FLAVOUR of text (email,
            // url, tel) is not modelled here — see the note on `Text`.
            Self::Text => Ok(()),
            Self::Number { min, max, step } => {
                check_integer(text, *min, *max, *step, &describe_number(*min, *max, *step))
            }
            Self::Range { min, max, step } => check_integer(
                text,
                Some(*min),
                Some(*max),
                Some(*step),
                &describe_number(Some(*min), Some(*max), Some(*step)),
            ),
            Self::Temporal { precision } => check_temporal(text, *precision),
            Self::Color => check_color(text),
            // Existence is the host's business and is racy by nature; the
            // protocol only asks that a path was actually named.
            Self::Path { .. } => (!text.is_empty())
                .then_some(())
                .ok_or_else(|| "a path".to_string()),
        }
    }

    /// What this kind advertises after a control's label — ` [y/n]`,
    /// ` [YYYY-MM-DD]`, ` (secret, not echoed)`.
    ///
    /// **One table, not one per surface.** The plain projection and the
    /// RichTUI view model each carried this suffix list, which is two places
    /// for one piece of knowledge and the shape the reuse discipline exists to
    /// stop: a kind added to one and forgotten in the other renders as a bare
    /// label on that surface, silently. Here a new kind advertises itself
    /// everywhere by construction.
    ///
    /// The strings for the pre-existing kinds are EXACT: the plain projection
    /// is a byte-identity contract, so `Toggle` is ` [y/n]` — never `[y/N]`,
    /// because a rendered default is how a headless surface chooses one by
    /// accident.
    ///
    /// [`Choice`](Self::Choice) advertises nothing here: its options are
    /// rendered as their own lines, not as a suffix.
    #[must_use]
    pub fn hint(&self) -> String {
        match self {
            Self::Choice { .. } => String::new(),
            Self::Text => String::new(),
            Self::Toggle => " [y/n]".to_string(),
            Self::Secret => " (secret, not echoed)".to_string(),
            Self::Number { min, max, step } => match (min, max) {
                (None, None) => " [number]".to_string(),
                _ => format!(" [{}]", describe_number(*min, *max, *step)),
            },
            Self::Range { min, max, .. } => format!(" [{min}..={max}]"),
            Self::Temporal { precision } => format!(" [{}]", precision.pattern()),
            Self::Color => " [#rrggbb]".to_string(),
            Self::Path { kind, .. } => match kind {
                PathKind::File => " [file path]".to_string(),
                PathKind::Directory => " [directory path]".to_string(),
            },
        }
    }

    /// Whether an answer to this control travels as `ControlValue::Text`.
    ///
    /// True for every value-shaped kind — text, number, range, temporal,
    /// color, path. False for the three that have their own value shape: a
    /// choice names an option, a toggle carries a bool, and a secret carries a
    /// REFERENCE (there is deliberately no variant that can hold plaintext).
    #[must_use]
    pub fn travels_as_text(&self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::Number { .. }
                | Self::Range { .. }
                | Self::Temporal { .. }
                | Self::Color
                | Self::Path { .. }
        )
    }
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

/// How a number control's value space reads in a refusal or a hint.
fn describe_number(min: Option<i64>, max: Option<i64>, step: Option<i64>) -> String {
    let range = match (min, max) {
        (Some(min), Some(max)) => format!("an integer in {min}..={max}"),
        (Some(min), None) => format!("an integer {min} or greater"),
        (None, Some(max)) => format!("an integer {max} or less"),
        (None, None) => "an integer".to_string(),
    };
    match step.filter(|s| *s > 1) {
        Some(step) => format!("{range}, in steps of {step}"),
        None => range,
    }
}

/// The shared integer check behind `Number` and `Range`.
///
/// Two hostile-definition hazards, both made unrepresentable rather than
/// caught. A `step` of zero or less is treated as "any integer in range": the
/// definition may come from untrusted markup, and dividing by a hostile zero
/// is a panic in a parser, while refusing every answer would hand its author a
/// denial of service. And the grid offset is computed in `i128`, because
/// `value - min` overflows `i64` for a control spanning the full range — a
/// `checked_sub` there would refuse `i64::MAX` as malformed when it is
/// perfectly in range and on grid. The wider type has no such edge to get
/// wrong.
fn check_integer(
    text: &str,
    min: Option<i64>,
    max: Option<i64>,
    step: Option<i64>,
    expected: &str,
) -> Result<(), String> {
    let value: i64 = text.parse().map_err(|_| expected.to_string())?;
    if min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max) {
        return Err(expected.to_string());
    }
    if let Some(step) = step.filter(|s| *s > 1) {
        let base = i128::from(min.unwrap_or(0));
        let offset = i128::from(value) - base;
        if offset.rem_euclid(i128::from(step)) != 0 {
            return Err(expected.to_string());
        }
    }
    Ok(())
}

/// `#rrggbb`, sRGB, lowercase or upper.
fn check_color(text: &str) -> Result<(), String> {
    let expected = || "#rrggbb".to_string();
    let hex = text.strip_prefix('#').ok_or_else(expected)?;
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(())
        .ok_or_else(expected)
}

/// A point in time, to the precision asked for.
///
/// Shape and field ranges, NOT calendar truth: `2026-02-31` passes here. A
/// calendar is a host concern (and a leap-second argument), and this
/// vocabulary's job is to refuse an answer that is not a date AT ALL.
fn check_temporal(text: &str, precision: TemporalPrecision) -> Result<(), String> {
    let expected = || precision.pattern().to_string();
    let (date, time) = match precision {
        TemporalPrecision::DateTime => {
            let (d, t) = text.split_once('T').ok_or_else(expected)?;
            (Some(d), Some(t))
        }
        TemporalPrecision::Time => (None, Some(text)),
        _ => (Some(text), None),
    };
    if let Some(date) = date {
        let mut parts = date.split('-');
        let year = parts.next().ok_or_else(expected)?;
        if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
            return Err(expected());
        }
        match precision {
            TemporalPrecision::Week => {
                let week = parts.next().ok_or_else(expected)?;
                let n = week.strip_prefix('W').ok_or_else(expected)?;
                number_in(n, 2, 1, 53).ok_or_else(expected)?;
            }
            TemporalPrecision::Month => {
                number_in(parts.next().ok_or_else(expected)?, 2, 1, 12).ok_or_else(expected)?;
            }
            _ => {
                number_in(parts.next().ok_or_else(expected)?, 2, 1, 12).ok_or_else(expected)?;
                number_in(parts.next().ok_or_else(expected)?, 2, 1, 31).ok_or_else(expected)?;
            }
        }
        if parts.next().is_some() {
            return Err(expected());
        }
    }
    if let Some(time) = time {
        let mut parts = time.split(':');
        number_in(parts.next().ok_or_else(expected)?, 2, 0, 23).ok_or_else(expected)?;
        number_in(parts.next().ok_or_else(expected)?, 2, 0, 59).ok_or_else(expected)?;
        // Seconds are optional at every precision that has a time at all.
        if let Some(seconds) = parts.next() {
            number_in(seconds, 2, 0, 60).ok_or_else(expected)?;
        }
        if parts.next().is_some() {
            return Err(expected());
        }
    }
    Ok(())
}

/// A fixed-width, zero-padded decimal field within `lo..=hi`.
fn number_in(field: &str, width: usize, lo: u32, hi: u32) -> Option<u32> {
    if field.len() != width || !field.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    field.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n))
}
