//! The tool-exposure controller (Pass 1) — the fourth pipeline stage.
//!
//! The catalog pipeline is four ordered stages:
//!
//! ```text
//! known      = registry.all()                      // every dispatchable name
//! present    = filter_presence(known, session)     // Gate / injected capability
//! authorized = filter_authority(present, disposition, caveats)
//! exposed    = exposure_policy.select(authorized, budget, active)   // THIS module
//! ```
//!
//! `present` is [`super::catalog::merged_tool_definitions`] (gate/presence),
//! Persona preferences rank the full catalog without removing tools.
//! [`super::catalog::filter_tools_for_disposition`] applies turn restrictions. This module
//! is the final `exposed` stage: it decides which *authorized* tools are worth a
//! schema slot given the model's LIVE usable budget.
//!
//! **Exposure is never authorization.** Dispatch still checks the authorized set
//! (disposition, explicit caveats, and MCP permission); hiding a schema to save tokens
//! never changes what the model may RUN, only what it is SHOWN. A model that
//! calls a real, authorized-but-unexposed tool is not hallucinating — see the
//! `ToolReach::KnownHidden` recovery reserved in `docs/design/tool-exposure-controller.md`.
//!
//! The primary control signal is the model's live usable budget (probed
//! `safe_context` → send budget), NOT its name. When no live budget signal
//! exists, the controller does not clip — no signal means no starvation.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::ExposureProfile;
use crate::tokens::TokenEstimation;

use super::catalog::BASE_TOOL_NAMES;

/// Maximum function tools accepted by the OpenAI-compatible function-calling
/// contract. This is a provider wire limit, not an authorization or operator
/// exposure preference: a `Full` profile may authorize more tools than can fit
/// in one request. Omitted tools remain authorized at dispatch and listable by
/// `tool_search`; this projection does not activate their schemas.
pub(crate) const OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS: usize = 128;

/// How a tool earns a schema slot, independent of whether it is *available*
/// (gate) or *authorized* (disposition/caveats). Pure data — the class
/// for each tool lives in [`EXPOSURE_CLASSES`], guarded against drift by
/// [`tests::every_known_tool_is_classified`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExposureClass {
    /// Session-critical loop control + fundamental workspace verbs. Never
    /// evicted for budget; always exposed when authorized + present.
    Kernel,
    /// Loaded when task intent / recent use suggests it. Evictable under budget.
    ByIntent,
    /// Surfaced when the backing artifact/context exists (event-gating is a
    /// later pass; for now these evict early under budget pressure).
    RecoveryOnly,
    /// Only after explicit discovery / a `KnownHidden` retry (MCP + deferred
    /// families). Not exposed until promoted.
    OnDemand,
}

/// The exposure class of every non-base built-in tool, by name. Base tools
/// (`BASE_TOOL_NAMES`) are [`ExposureClass::Kernel`] by convention; unknown /
/// MCP (`server__tool`) names are [`ExposureClass::OnDemand`]. This is the ONE
/// place a tool's class is declared — [`tests::every_known_tool_is_classified`]
/// asserts it covers exactly the known catalog so a new tool cannot ship
/// unclassified.
const EXPOSURE_CLASSES: &[(&str, ExposureClass)] = &[
    // Kernel: discovery must always be present so a budget-clipped model can
    // still find what was hidden.
    ("tool_search", ExposureClass::Kernel),
    // Any tool result can spill, before intent or recent use can promote its
    // reader. Keep recovery callable whenever its source and authority allow it.
    ("memory_fetch", ExposureClass::Kernel),
    // Recovery affordances — only meaningful when their artifact/context exists.
    ("resume_context", ExposureClass::RecoveryOnly),
    ("prompt_read", ExposureClass::RecoveryOnly),
    ("artifact_read", ExposureClass::RecoveryOnly),
    ("get_context_remaining", ExposureClass::RecoveryOnly),
    ("request_user_input", ExposureClass::RecoveryOnly),
    ("render_report", ExposureClass::RecoveryOnly),
    // ByIntent: loaded when the task points at them.
    ("lifecycle", ExposureClass::ByIntent),
    ("save_note", ExposureClass::ByIntent),
    ("recall", ExposureClass::ByIntent),
    ("git", ExposureClass::ByIntent),
    ("compose_roster", ExposureClass::ByIntent),
    ("crew", ExposureClass::ByIntent),
    ("state_set", ExposureClass::ByIntent),
    ("state_get", ExposureClass::ByIntent),
    ("state_clear", ExposureClass::ByIntent),
    ("code_search", ExposureClass::ByIntent),
    // #1387 Code Navigator — inspection/navigation intent.
    ("where_is", ExposureClass::ByIntent),
    ("goto_definition", ExposureClass::ByIntent),
    ("text_search", ExposureClass::ByIntent),
    ("find_references", ExposureClass::ByIntent),
    ("find_tests", ExposureClass::ByIntent),
    ("find_callers", ExposureClass::ByIntent),
    ("find_callees", ExposureClass::ByIntent),
    ("find_implementations", ExposureClass::ByIntent),
    ("find_hierarchy", ExposureClass::ByIntent),
    ("inspect_type", ExposureClass::ByIntent),
    ("impact", ExposureClass::ByIntent),
    ("experience_record", ExposureClass::ByIntent),
    ("experience_recall", ExposureClass::ByIntent),
    ("update_plan", ExposureClass::ByIntent),
    ("plan_get", ExposureClass::ByIntent),
    ("enter_plan_mode", ExposureClass::ByIntent),
    ("exit_plan_mode", ExposureClass::ByIntent),
    ("select_operating_mode", ExposureClass::ByIntent),
];

/// The exposure class of a tool by name. Base tools are `Kernel`; a name found
/// in [`EXPOSURE_CLASSES`] takes its declared class; everything else (MCP
/// `server__tool`, genuinely unknown) is `OnDemand`.
#[must_use]
pub fn classify(name: &str) -> ExposureClass {
    if BASE_TOOL_NAMES.contains(&name) {
        return ExposureClass::Kernel;
    }
    EXPOSURE_CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
        .unwrap_or(ExposureClass::OnDemand)
}

/// Tools hidden when [`ExposureSettings::read_only`] is `true`. These are the
/// filesystem mutation tools; `run_command` is NOT included — a read-only view
/// may still need to run read-only commands (e.g. `cargo check`, `grep`).
pub(crate) const WRITE_TOOL_NAMES: &[&str] = &["write_file", "edit_file", "delete_file"];

/// The order bands fill remaining budget after Kernel + active. Lower index =
/// kept first. Kernel is mandatory (not in this list); active tools are kept
/// regardless of class before any band fill.
const FILL_ORDER: &[ExposureClass] = &[
    ExposureClass::ByIntent,
    ExposureClass::RecoveryOnly,
    ExposureClass::OnDemand,
];

/// The resolved `[tool_exposure]` policy carried on `ChatCtx`. Default is
/// [`ExposureProfile::Full`] — the identity controller (bit-for-bit unchanged
/// advertised set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposureSettings {
    pub profile: ExposureProfile,
    /// Percent of the live usable budget spent on tool schemas (`auto`).
    pub schema_budget_pct: u16,
    /// Hard cap on exposed tool count (0 = unlimited). A safety rail, not the
    /// governor — the budget is.
    pub max_initial_tools: usize,
    /// When `Some`, only the named tools are shown to the model — the
    /// operator's explicit final-say filter, applied after all other exposure
    /// logic including Kernel classification. Absent = no allow-list filter.
    pub allow_list: Option<Vec<String>>,
    /// When `true`, `write_file`, `edit_file`, and `delete_file` are hidden
    /// regardless of `profile` or `allow_list`.
    pub read_only: bool,
    /// The operator's own system prompt file is in force (#2938): the request
    /// carries that text as the only system message, so the harness adds none
    /// of its own (active-prompt card, observed facts, output planning).
    pub verbatim_system: bool,
    /// `[prompt] components` switched `card` off (#2933): the driver adds no
    /// active-prompt card, and only that; the rest of the harness text stays.
    pub omit_card: bool,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        Self {
            profile: ExposureProfile::Full,
            schema_budget_pct: 15,
            max_initial_tools: 0,
            allow_list: None,
            read_only: false,
            verbatim_system: false,
            omit_card: false,
        }
    }
}

impl From<crate::config::ToolExposureConfig> for ExposureSettings {
    fn from(c: crate::config::ToolExposureConfig) -> Self {
        Self {
            profile: c.profile,
            schema_budget_pct: c.schema_budget_pct,
            max_initial_tools: c.max_initial_tools,
            allow_list: c.allow_list,
            read_only: c.read_only,
            verbatim_system: false,
            omit_card: false,
        }
    }
}

impl ExposureSettings {
    /// Whether the operator's `allow_list` / `read_only` policy lets `name`
    /// reach the model at all. The ONE predicate behind catalog selection,
    /// discovery, promotion and prompt rendering, so none of them can name a
    /// tool another has hidden (#2932).
    #[must_use]
    pub fn operator_visible(&self, name: &str) -> bool {
        let listed = self
            .allow_list
            .as_ref()
            .is_none_or(|list| list.iter().any(|allowed| allowed == name));
        listed && !(self.read_only && WRITE_TOOL_NAMES.contains(&name))
    }

    /// Whether any operator policy is in force (absent = identity).
    fn has_operator_filter(&self) -> bool {
        self.allow_list.is_some() || self.read_only
    }

    /// Narrow a session's authority to match `read_only`: a meet with
    /// [`read_only_clamp`](super::read_only_clamp), so visibility and authority
    /// cannot drift apart. Only narrows; `read_only = false` is the identity.
    #[must_use]
    pub fn attenuate(&self, caveats: crate::caveats::Caveats) -> crate::caveats::Caveats {
        if self.read_only {
            caveats.meet(&super::read_only_clamp())
        } else {
            caveats
        }
    }

    /// Render an identity prompt so it names only operator-visible tools: the
    /// `You have tools: …` line is rewritten to the visible subset and any
    /// paragraph that names a hidden built-in is dropped. Identity without an
    /// operator filter is returned unchanged.
    #[must_use]
    pub fn render_identity<'a>(&self, identity: &'a str) -> std::borrow::Cow<'a, str> {
        self.render_identity_with(identity, &crate::prompt_components::ComponentSet::default())
    }

    /// [`render_identity`](Self::render_identity) with `[prompt] components` applied.
    /// Components shape the default soul; any other identity (a custom `soul.md`,
    /// the coach soul) is filtered as text.
    #[must_use]
    pub fn render_identity_with<'a>(
        &self,
        identity: &'a str,
        components: &crate::prompt_components::ComponentSet,
    ) -> std::borrow::Cow<'a, str> {
        if identity == crate::DEFAULT_SOUL {
            if !self.has_operator_filter() && components.is_default() {
                return std::borrow::Cow::Borrowed(identity);
            }
            return std::borrow::Cow::Owned(crate::prompt_components::render_soul(
                &|c| components.is_on(c),
                &|tool| self.operator_visible(tool),
            ));
        }
        if !self.has_operator_filter() {
            return std::borrow::Cow::Borrowed(identity);
        }
        self.render_text_identity(identity)
    }

    /// The text-matching filter, for identities that are not the registry's soul
    /// (a custom `soul.md`, the coach soul, a persona body).
    fn render_text_identity<'a>(&self, identity: &'a str) -> std::borrow::Cow<'a, str> {
        let hidden_names = |text: &str| {
            super::catalog::ALL_TOOL_NAMES
                .iter()
                .any(|name| !self.operator_visible(name) && mentions_tool(text, name))
        };
        let paragraphs: Vec<String> = identity
            .split("\n\n")
            .filter_map(|paragraph| {
                // The tool sentence may sit mid-paragraph ("You are newt… You
                // have tools: …"): rewrite it in place, then judge the rest.
                let rewritten = match paragraph.find(TOOL_LINE_PREFIX) {
                    Some(at) => {
                        let (head, rest) = paragraph.split_at(at + TOOL_LINE_PREFIX.len());
                        let (names, tail) = rest.split_once('.').unwrap_or((rest, ""));
                        let kept: Vec<&str> = names
                            .split(',')
                            .map(str::trim)
                            .filter(|name| !name.is_empty() && self.operator_visible(name))
                            .collect();
                        format!("{head}{}.{tail}", kept.join(", "))
                    }
                    None => paragraph.to_owned(),
                };
                (!hidden_names(&rewritten)).then_some(rewritten)
            })
            .collect();
        std::borrow::Cow::Owned(paragraphs.join("\n\n"))
    }
}

use crate::prompt_components::TOOL_LINE_PREFIX;

/// Whether `text` names `tool` as a whole word (`edit_file`, not `edit_files`).
fn mentions_tool(text: &str, tool: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(tool).any(|(at, _)| {
        let before = text[..at].chars().next_back().is_none_or(|c| !is_word(c));
        let after = text[at + tool.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_word(c));
        before && after
    })
}

/// What the controller decided this turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposurePlan {
    /// Names kept on the wire, in the catalog's original order.
    pub exposed: Vec<String>,
    /// Authorized names dropped from the wire (still dispatchable). They seed
    /// the turn's [`HiddenTools`], which `tool_search` and dispatch read.
    pub hidden: Vec<String>,
    /// Names the operator's `allow_list` / `read_only` policy excludes. They
    /// are never discoverable or promotable: a call is refused, a search does
    /// not list them (#2932).
    pub operator_hidden: Vec<String>,
}

/// How `tool_search` marks an authorized tool whose schema is off the wire.
pub(crate) const HIDDEN_SEARCH_MARK: &str =
    "[schema not loaded: use tool_search with this exact name to load it]";

/// What a call to a hidden tool did (#2331).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Promotion {
    /// Not an authorized off-wire tool: dispatch continues.
    NotHidden,
    /// Its schema rides the next request.
    Promoted,
    /// The request is already at this many function tools, the wire's cap.
    AtCap(usize),
    /// The operator's tool policy excludes it: never loaded, never run (#2932).
    OperatorHidden,
}

impl Promotion {
    /// The dispatch result, or `None` when dispatch continues. The call never
    /// runs: no arguments run against a schema the model never saw.
    pub(crate) fn message(self, name: &str) -> Option<String> {
        match self {
            Self::NotHidden => None,
            Self::OperatorHidden => Some(format!(
                "Tool `{name}` is not available in this session: the operator's tool policy \
                 excludes it, and it cannot be loaded. Use the tools you were given."
            )),
            Self::Promoted => Some(format!(
                "Tool `{name}` did not run (schema not loaded). Its schema is loaded for your \
                 next request; call `{name}` with arguments that match it."
            )),
            Self::AtCap(cap) => Some(format!(
                "Tool `{name}` did not run and could not be loaded: the tool list is at its limit \
                 of {cap} function tools, with no replaceable slot. Kernel tools and activations \
                 already queued for the next request are protected."
            )),
        }
    }
}

/// The turn's authorized-but-unexposed tools (#2331). Exact-name search or a
/// hidden call queues its schema for the next request without executing it.
/// Below the wire cap schemas append; at the cap the oldest optional schema
/// moves back here. Kernel tools and queued activations cannot be displaced.
#[derive(Debug, Default)]
pub(crate) struct HiddenTools {
    state: std::sync::Mutex<HiddenState>,
}

#[derive(Debug, Default)]
struct HiddenState {
    /// Hidden definitions, in catalog order.
    defs: Vec<Value>,
    /// The operator's policy, resolved once per turn: the ceiling every other
    /// set is drawn under. Held as the policy, not as a list of definitions,
    /// so a tool an earlier stage already dropped (Plan's disposition filter)
    /// is still refused on call and absent from discovery (#2932).
    policy: ExposureSettings,
    /// Names activated since the last append, in request order.
    promoted: Vec<String>,
    /// `(cap, sent)`: the wire cap and its reserved next-request definitions,
    /// in order. Keep the actual narrowed schemas, not a rebuilt catalog.
    cap: Option<(usize, Vec<Value>)>,
}

impl HiddenTools {
    fn state(&self) -> std::sync::MutexGuard<'_, HiddenState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `name` is authorized but not on the wire.
    pub(crate) fn is_hidden(&self, name: &str) -> bool {
        self.state()
            .defs
            .iter()
            .any(|def| entry_name(def) == Some(name))
    }

    /// Whether the operator's tool policy excludes `name` this turn (#2932).
    /// Such a tool is neither discoverable nor promotable.
    pub(crate) fn is_operator_hidden(&self, name: &str) -> bool {
        !self.state().policy.operator_visible(name)
    }

    /// Register the actual wire projection so replacements preserve its
    /// narrowed definitions and can never displace Kernel schemas.
    fn limit_to(&self, cap: usize, sent: &[Value]) {
        self.state().cap = Some((cap, sent.to_vec()));
    }

    /// Reserve a slot for a hidden schema. At capacity replace the oldest
    /// unqueued optional schema; a refused tool never enters this state.
    pub(crate) fn promote(&self, name: &str) -> Promotion {
        let mut state = self.state();
        if !state.policy.operator_visible(name) {
            return Promotion::OperatorHidden;
        }
        let Some(requested) = state
            .defs
            .iter()
            .find(|def| entry_name(def) == Some(name))
            .cloned()
        else {
            return Promotion::NotHidden;
        };
        if state.promoted.iter().any(|promoted| promoted == name) {
            return Promotion::Promoted;
        }
        let HiddenState {
            defs,
            promoted,
            cap,
            ..
        } = &mut *state;
        if let Some((cap, sent)) = cap {
            if sent.len() >= *cap {
                let Some(at) = sent.iter().position(|def| {
                    entry_name(def).is_some_and(|name| {
                        classify(name) != ExposureClass::Kernel
                            && !promoted.iter().any(|queued| queued == name)
                    })
                }) else {
                    return Promotion::AtCap(*cap);
                };
                // A reserved victim is immediately discoverable/reactivatable;
                // later requests in this batch still cannot steal queued slots.
                defs.push(sent.remove(at));
            }
            sent.push(requested);
        }
        promoted.push(name.to_owned());
        Promotion::Promoted
    }

    /// Remove reserved victims and append queued schemas in activation order.
    /// Returns whether the tool list changed; the loop reprices the result.
    pub(crate) fn append_promoted(&self, tools: &mut Value) -> bool {
        let mut state = self.state();
        if state.promoted.is_empty() {
            return false;
        }
        let Value::Array(sent) = tools else {
            return false;
        };
        if let Some((_, next)) = &state.cap {
            sent.retain(|def| {
                let name = entry_name(def);
                next.iter().any(|kept| entry_name(kept) == name)
                    && !state
                        .promoted
                        .iter()
                        .any(|queued| Some(queued.as_str()) == name)
            });
        }
        for name in std::mem::take(&mut state.promoted) {
            if let Some(at) = state
                .defs
                .iter()
                .position(|def| entry_name(def) == Some(&name))
            {
                sent.push(state.defs.remove(at));
            }
        }
        true
    }
}

/// The name of a catalog entry (`{"function":{"name":…}}`), or `None`.
fn entry_name(def: &Value) -> Option<&str> {
    def.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
}

/// Compute the schema-token budget from the live usable budget and profile.
/// `Full` → `None` (identity). `Auto`/`Minimal` scale `live_budget` by the
/// configured percent; `None` live budget under `Auto` yields `None` (do not
/// clip without a signal), while `Minimal` yields `Some(0)` (kernel+active only).
fn budget_tokens(settings: &ExposureSettings, live_budget_tokens: Option<usize>) -> Option<usize> {
    match settings.profile {
        ExposureProfile::Full => None,
        ExposureProfile::Auto => {
            live_budget_tokens.map(|b| b.saturating_mul(settings.schema_budget_pct as usize) / 100)
        }
        ExposureProfile::Minimal => Some(
            live_budget_tokens
                .map(|b| b.saturating_mul(settings.schema_budget_pct as usize) / 100)
                .unwrap_or(0),
        ),
    }
}

/// Apply `allow_list` and `read_only` operator filters to a name list,
/// returning an [`ExposurePlan`]. Pure; used both on the budget-free path and
/// after budget-based selection.
fn apply_operator_filters(all_names: Vec<String>, settings: &ExposureSettings) -> ExposurePlan {
    if !settings.has_operator_filter() {
        return ExposurePlan {
            exposed: all_names,
            hidden: Vec::new(),
            operator_hidden: Vec::new(),
        };
    }
    let (exposed, operator_hidden): (Vec<String>, Vec<String>) = all_names
        .into_iter()
        .partition(|name| settings.operator_visible(name));
    ExposurePlan {
        exposed,
        hidden: Vec::new(),
        operator_hidden,
    }
}

/// Plan which authorized tools to expose given the live budget and the sticky
/// `active` working set. Pure over the catalog `Value`; the actual filtering is
/// [`select_exposed`]. `Full` (or a non-array catalog) exposes everything.
#[must_use]
pub fn plan_exposure(
    defs: &Value,
    settings: &ExposureSettings,
    live_budget_tokens: Option<usize>,
    active: &BTreeSet<String>,
    est: TokenEstimation,
) -> ExposurePlan {
    let all_names: Vec<String> = defs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| entry_name(d).map(str::to_owned))
        .collect();

    // The operator ceiling comes FIRST: every set below (budget-exposed,
    // budget-deferred) is drawn from the operator-visible names only, so a
    // hidden tool can never enter the promotable pool (#2932).
    let mut plan = apply_operator_filters(all_names, settings);

    // Identity: Full profile, non-array catalog, or no budget under Auto.
    let Some(budget) = budget_tokens(settings, live_budget_tokens) else {
        return plan;
    };
    let entries: Vec<&Value> = defs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|def| entry_name(def).is_none_or(|name| settings.operator_visible(name)))
        .collect();

    let mut kept: BTreeSet<String> = BTreeSet::new();
    let mut kept_tokens = 0usize;

    // Mandatory floor: Kernel + sticky active + any nameless (malformed) entry.
    // These are exempt from both the budget and `max_initial_tools`.
    for def in &entries {
        let Some(name) = entry_name(def) else {
            continue;
        };
        let mandatory = classify(name) == ExposureClass::Kernel || active.contains(name);
        if mandatory && kept.insert(name.to_owned()) {
            kept_tokens += crate::agentic::trim::estimate_value_tokens(def, est);
        }
    }

    // Budget-order fill. Kernel + active are already in `kept` (and stay even
    // when they exceed the cap). The cap, when non-zero, is a total-count rail
    // on the final exposed set — additional fill stops once `kept.len()` hits it.
    let cap = settings.max_initial_tools;
    for band in FILL_ORDER {
        for def in &entries {
            let Some(name) = entry_name(def) else {
                continue;
            };
            if kept.contains(name) || classify(name) != *band {
                continue;
            }
            if cap != 0 && kept.len() >= cap {
                break;
            }
            let cost = crate::agentic::trim::estimate_value_tokens(def, est);
            if kept_tokens.saturating_add(cost) > budget {
                continue;
            }
            kept.insert(name.to_owned());
            kept_tokens += cost;
        }
    }

    // Split the operator-visible names by the budget's choice. The deferred
    // half stays authorized at dispatch and reachable via tool_search /
    // KnownHidden promotion; the operator-hidden half was set aside above.
    let (exposed, hidden) = std::mem::take(&mut plan.exposed)
        .into_iter()
        .partition(|name| kept.contains(name.as_str()));
    plan.exposed = exposed;
    plan.hidden = hidden;
    plan
}

/// Apply the exposure policy to an authorized catalog, returning the exposed
/// subset in original order. `Full` (or a non-array catalog) without operator
/// filters is identity. This is the single seam the three agentic loops call
/// after disposition filtering and before the token estimate, so what is
/// counted and what is sent agree.
#[must_use]
pub fn select_exposed(
    defs: Value,
    settings: &ExposureSettings,
    live_budget_tokens: Option<usize>,
    active: &BTreeSet<String>,
    est: TokenEstimation,
) -> (Value, HiddenTools) {
    // Full profile with no operator filters is the identity — no allocation.
    if settings.profile == ExposureProfile::Full
        && settings.allow_list.is_none()
        && !settings.read_only
    {
        return (defs, HiddenTools::default());
    }
    let Value::Array(arr) = defs else {
        return (defs, HiddenTools::default());
    };
    let plan = plan_exposure(
        &Value::Array(arr.clone()),
        settings,
        live_budget_tokens,
        active,
        est,
    );
    let hidden: BTreeSet<&str> = plan.hidden.iter().map(String::as_str).collect();
    let operator: BTreeSet<&str> = plan.operator_hidden.iter().map(String::as_str).collect();
    let mut exposed = Vec::new();
    let mut deferred = Vec::new();
    for def in arr {
        match entry_name(&def) {
            // Operator-hidden definitions are dropped: the policy, not a
            // copy of the schema, is what refuses them later.
            Some(name) if operator.contains(name) => {}
            Some(name) if hidden.contains(name) => deferred.push(def),
            _ => exposed.push(def),
        }
    }
    let hidden = HiddenTools {
        state: std::sync::Mutex::new(HiddenState {
            defs: deferred,
            policy: settings.clone(),
            promoted: Vec::new(),
            cap: None,
        }),
    };
    (Value::Array(exposed), hidden)
}

/// Project an authorized/exposed catalog onto the OpenAI-compatible wire's
/// 128-function envelope.
///
/// Provider shape is the final pipeline constraint after exposure. Kernel
/// tools are selected first regardless of their current array position, then
/// the ordinary exposure bands fill the remaining slots. The returned array
/// preserves original catalog order so the projection is deterministic and
/// does not churn prompt prefixes. Authorization is unchanged: tools omitted
/// here remain governed by the dispatch boundary and listable through
/// `tool_search`, without implying that this projection activates them.
#[must_use]
pub(crate) fn select_openai_compatible_tools(defs: Value, hidden: &HiddenTools) -> Value {
    let Value::Array(arr) = defs else {
        return defs;
    };
    if arr.len() <= OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
        hidden.limit_to(OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS, &arr);
        return Value::Array(arr);
    }

    let kernel_count = arr
        .iter()
        .filter_map(entry_name)
        .filter(|name| classify(name) == ExposureClass::Kernel)
        .count();
    debug_assert!(
        kernel_count <= OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS,
        "kernel tool catalog exceeds the OpenAI-compatible wire envelope"
    );

    let mut keep = vec![false; arr.len()];
    let mut kept = 0usize;
    for class in [
        ExposureClass::Kernel,
        ExposureClass::ByIntent,
        ExposureClass::RecoveryOnly,
        ExposureClass::OnDemand,
    ] {
        for (index, def) in arr.iter().enumerate() {
            if kept == OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
                break;
            }
            if entry_name(def).is_some_and(|name| classify(name) == class) {
                keep[index] = true;
                kept += 1;
            }
        }
    }

    let mut sent = Vec::with_capacity(kept);
    {
        let mut state = hidden.state();
        for (def, keep) in arr.into_iter().zip(keep) {
            if keep {
                sent.push(def);
            } else {
                state.defs.push(def);
            }
        }
    }
    hidden.limit_to(OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS, &sent);
    Value::Array(sent)
}

#[cfg(test)]
mod tests {
    use super::super::catalog::ALL_TOOL_NAMES;
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    fn est() -> TokenEstimation {
        TokenEstimation::default()
    }

    #[test]
    fn every_known_tool_is_classified() {
        // Anti-drift: every dispatchable built-in must resolve to a class, and
        // no `EXPOSURE_CLASSES` row may name a tool that no longer exists.
        for name in ALL_TOOL_NAMES.iter() {
            let _ = classify(name); // never panics; asserts total coverage below
        }
        for (name, _) in EXPOSURE_CLASSES {
            assert!(
                ALL_TOOL_NAMES.contains(name),
                "EXPOSURE_CLASSES names `{name}` which is not a known tool"
            );
        }
        // Every non-base known tool must be explicitly listed (not defaulted to
        // OnDemand by accident) so a new tool ships with a deliberate class.
        for name in ALL_TOOL_NAMES.iter() {
            if BASE_TOOL_NAMES.contains(name) {
                continue;
            }
            assert!(
                EXPOSURE_CLASSES.iter().any(|(n, _)| n == name),
                "tool `{name}` has no explicit exposure class — add it to EXPOSURE_CLASSES"
            );
        }
    }

    #[test]
    fn base_tools_and_unknown_classify_by_convention() {
        assert_eq!(classify("read_file"), ExposureClass::Kernel);
        assert_eq!(classify("run_command"), ExposureClass::Kernel);
        assert_eq!(classify("tool_search"), ExposureClass::Kernel);
        assert_eq!(classify("git"), ExposureClass::ByIntent);
        assert_eq!(classify("resume_context"), ExposureClass::RecoveryOnly);
        assert_eq!(classify("github__create_issue"), ExposureClass::OnDemand);
        assert_eq!(classify("totally_made_up"), ExposureClass::OnDemand);
    }

    #[test]
    fn full_profile_is_identity() {
        let defs = json!([
            tool("read_file", "x"),
            tool("git", "y"),
            tool("impact", "z")
        ]);
        let out = select_exposed(
            defs.clone(),
            &ExposureSettings::default(),
            Some(100),
            &BTreeSet::new(),
            est(),
        )
        .0;
        assert_eq!(out, defs, "Full must be bit-for-bit identity");
    }

    #[test]
    fn queued_activations_keep_kernel_and_can_reactivate_evicted_schemas() {
        let hidden = HiddenTools {
            state: std::sync::Mutex::new(HiddenState {
                defs: vec![
                    tool("remote__a", "a"),
                    tool("remote__b", "b"),
                    tool("remote__c", "c"),
                ],
                ..Default::default()
            }),
        };
        let mut sent = json!([
            tool("read_file", "kernel"),
            tool("git", "oldest optional"),
            tool("tool_search", "kernel"),
            tool("impact", "optional")
        ]);
        hidden.limit_to(4, sent.as_array().unwrap());
        assert_eq!(hidden.promote("unknown__tool"), Promotion::NotHidden);
        assert_eq!(hidden.promote("remote__a"), Promotion::Promoted);
        assert_eq!(hidden.promote("remote__a"), Promotion::Promoted);
        assert_eq!(hidden.promote("remote__b"), Promotion::Promoted);
        assert_eq!(hidden.promote("remote__c"), Promotion::AtCap(4));
        assert_eq!(hidden.promote("git"), Promotion::AtCap(4));
        assert!(hidden.append_promoted(&mut sent));
        assert_eq!(
            sent,
            json!([
                tool("read_file", "kernel"),
                tool("tool_search", "kernel"),
                tool("remote__a", "a"),
                tool("remote__b", "b")
            ])
        );
        assert!(hidden.is_hidden("git"));
        assert!(hidden.is_hidden("impact"));
        assert!(!hidden.is_hidden("remote__a"));
        assert!(!hidden.append_promoted(&mut sent));

        assert_eq!(hidden.promote("git"), Promotion::Promoted);
        // Reactivate a just-reserved victim in the same batch. Only another
        // unqueued optional slot may move; neither queued promise is stolen.
        assert_eq!(hidden.promote("remote__a"), Promotion::Promoted);
        assert_eq!(hidden.promote("remote__b"), Promotion::AtCap(4));
        assert!(hidden.append_promoted(&mut sent));
        assert_eq!(
            sent,
            json!([
                tool("read_file", "kernel"),
                tool("tool_search", "kernel"),
                tool("git", "oldest optional"),
                tool("remote__a", "a")
            ])
        );
        assert!(hidden.is_hidden("remote__b"));
        assert!(!hidden.is_hidden("git"));
        assert_eq!(hidden.promote("git"), Promotion::NotHidden);
        assert!(!hidden.append_promoted(&mut sent));
    }

    #[test]
    fn openai_wire_cap_keeps_kernel_before_optional_mcp_tools() {
        let mut defs = vec![tool("optional_server__tool_000", "remote")];
        defs.extend(
            (1..=OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS)
                .map(|i| tool(&format!("optional_server__tool_{i:03}"), "remote")),
        );
        // Put kernel tools at the end to prove selection is by exposure law,
        // not an accidental `truncate(128)` over today's catalog order.
        defs.push(tool("run_command", "kernel"));
        defs.push(tool("tool_search", "kernel"));

        let out = select_openai_compatible_tools(Value::Array(defs), &HiddenTools::default());
        let names = out
            .as_array()
            .unwrap()
            .iter()
            .filter_map(entry_name)
            .collect::<Vec<_>>();

        assert_eq!(names.len(), OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS);
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"tool_search"));
    }

    #[test]
    fn auto_without_live_budget_is_identity() {
        let defs = json!([tool("read_file", "x"), tool("git", "y")]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            ..Default::default()
        };
        let out = select_exposed(defs.clone(), &settings, None, &BTreeSet::new(), est()).0;
        assert_eq!(out, defs, "no live signal => no clipping");
    }

    #[test]
    fn auto_keeps_kernel_even_when_budget_is_tiny() {
        let defs = json!([
            tool("read_file", "kernel"),
            tool("tool_search", "kernel"),
            tool("git", "byintent"),
            tool("impact", "byintent"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 1,
            max_initial_tools: 0,
            ..Default::default()
        };
        // Budget = 1% of 10 = 0 tokens: only Kernel survives.
        let plan = plan_exposure(&defs, &settings, Some(10), &BTreeSet::new(), est());
        assert!(plan.exposed.contains(&"read_file".to_string()));
        assert!(plan.exposed.contains(&"tool_search".to_string()));
        assert!(plan.hidden.contains(&"git".to_string()));
        assert!(plan.hidden.contains(&"impact".to_string()));
    }

    #[test]
    fn memory_fetch_survives_minimal_and_budget_clipped_exposure_when_authorized() {
        // Any tool result can spill, including on the first turn before this
        // retrieval tool has entered the sticky active set.
        let defs = json!([
            tool("read_file", "kernel"),
            tool("memory_fetch", "recover offloaded results"),
            tool("git", "optional"),
        ]);
        for (profile, budget) in [
            (ExposureProfile::Minimal, None),
            (ExposureProfile::Auto, Some(10)),
        ] {
            let settings = ExposureSettings {
                profile,
                schema_budget_pct: 1,
                max_initial_tools: 1,
                ..Default::default()
            };
            let out = select_exposed(defs.clone(), &settings, budget, &BTreeSet::new(), est()).0;
            assert_eq!(
                out,
                json!([defs[0], defs[1]]),
                "recovery must survive both schema and count budgets"
            );

            // Presence and authority are upstream: exposure must not create
            // a memory_fetch entry when those gates excluded it.
            let unavailable = json!([defs[0], defs[2]]);
            assert_eq!(
                select_exposed(unavailable, &settings, budget, &BTreeSet::new(), est()).0,
                json!([defs[0]])
            );
        }
    }

    #[test]
    fn active_tools_are_sticky_across_budget() {
        let defs = json!([tool("read_file", "kernel"), tool("git", "byintent")]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Minimal,
            schema_budget_pct: 1,
            max_initial_tools: 0,
            ..Default::default()
        };
        let mut active = BTreeSet::new();
        active.insert("git".to_string());
        let plan = plan_exposure(&defs, &settings, Some(0), &active, est());
        assert!(
            plan.exposed.contains(&"git".to_string()),
            "a recently-used tool must not vanish on a budget shift"
        );
    }

    #[test]
    fn auto_fills_by_budget_and_preserves_order() {
        // Big budget: everything fits; order preserved.
        let defs = json!([
            tool("read_file", "k"),
            tool("git", "b"),
            tool("impact", "b"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 100,
            max_initial_tools: 0,
            ..Default::default()
        };
        let out = select_exposed(
            defs.clone(),
            &settings,
            Some(100_000),
            &BTreeSet::new(),
            est(),
        )
        .0;
        assert_eq!(out, defs);
    }

    #[test]
    fn reduces_a_realistic_catalog_to_a_pocket_multitool() {
        // The proof a live 3.4k model cares about: over the REAL merged catalog
        // with every capability on, a tight budget keeps kernel + fits some
        // ByIntent, drops the rest, and the exposed set costs strictly less than
        // the full catalog — while nothing kernel is ever dropped.
        let full = super::super::catalog::merged_tool_definitions(
            &crate::agentic::NoMcp,
            true,
            true,
            true,
            Some(&crate::caveats::Scope::All),
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            Default::default(),
            crate::agentic::shell_dialect_sentence(false, false),
        );
        let full_tokens = crate::agentic::trim::estimate_value_tokens(&full, est());
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 15,
            max_initial_tools: 0,
            ..Default::default()
        };
        // A small model: ~4k usable tokens → ~600-token schema budget.
        let plan = plan_exposure(&full, &settings, Some(4_000), &BTreeSet::new(), est());
        let (exposed, _) = select_exposed(
            full.clone(),
            &settings,
            Some(4_000),
            &BTreeSet::new(),
            est(),
        );
        let exposed_tokens = crate::agentic::trim::estimate_value_tokens(&exposed, est());
        assert!(
            exposed_tokens < full_tokens,
            "exposed schema ({exposed_tokens}) must cost less than the full catalog ({full_tokens})"
        );
        // NOTE: the exposed cost is NOT asserted <= budget here — Kernel (which
        // includes the large base `run_command` schema) is exempt from the
        // budget by design. `auto_keeps_kernel_even_when_budget_is_tiny` covers
        // the budget-clip of the evictable (ByIntent) band deterministically.
        assert!(
            plan.exposed.iter().any(|n| n == "tool_search"),
            "discovery must survive so the model can find what was hidden"
        );
        assert!(
            !plan.hidden.is_empty(),
            "a tight budget over the full catalog must hide something"
        );
        // Every kernel tool present in the catalog must be exposed.
        for def in full.as_array().unwrap() {
            if let Some(name) = entry_name(def) {
                if classify(name) == ExposureClass::Kernel {
                    assert!(
                        plan.exposed.iter().any(|n| n == name),
                        "kernel tool `{name}` must never be evicted"
                    );
                }
            }
        }
    }

    /// #2950 review 1: an operator exclusion is a ceiling, not a deferral. A
    /// direct call or exact-name search must never load its schema.
    #[test]
    fn an_operator_hidden_tool_is_refused_not_promoted() {
        let defs = json!([
            tool("read_file", "k"),
            tool("write_file", "k"),
            tool("save_note", "n"),
        ]);
        let settings = ExposureSettings {
            allow_list: Some(vec!["read_file".to_string()]),
            ..Default::default()
        };
        let (exposed, hidden) = select_exposed(defs, &settings, None, &BTreeSet::new(), est());
        assert_eq!(exposed.as_array().unwrap().len(), 1);
        for name in ["write_file", "save_note"] {
            assert_eq!(hidden.promote(name), Promotion::OperatorHidden, "{name}");
            let message = hidden.promote(name).message(name).unwrap();
            assert!(message.contains("not available"), "{message}");
        }
        let mut tools = json!([tool("read_file", "k")]);
        assert!(
            !hidden.append_promoted(&mut tools),
            "nothing was queued, so the next request's tool list is unchanged"
        );
        assert_eq!(tools.as_array().unwrap().len(), 1);
    }

    /// #2950 review 2 (P1): the operator ceiling is applied BEFORE budget
    /// selection, so a tool both the budget and the operator would drop is
    /// operator-hidden (refused), never budget-deferred (promotable).
    #[test]
    fn budget_deferral_never_admits_an_operator_hidden_tool() {
        let defs = json!([
            tool("read_file", "k"),
            tool("tool_search", "k"),
            tool("save_note", "n"),
        ]);
        let allow = Some(vec!["read_file".to_string(), "tool_search".to_string()]);
        for profile in [ExposureProfile::Minimal, ExposureProfile::Auto] {
            let settings = ExposureSettings {
                profile,
                schema_budget_pct: 0,
                allow_list: allow.clone(),
                ..Default::default()
            };
            let plan = plan_exposure(&defs, &settings, Some(1_000), &BTreeSet::new(), est());
            assert!(
                !plan.hidden.iter().any(|name| name == "save_note"),
                "{profile:?}: the deferred pool is a subset of the visible set: {:?}",
                plan.hidden
            );
            assert_eq!(
                plan.operator_hidden,
                vec!["save_note".to_string()],
                "{profile:?}"
            );
            let (_, hidden) = select_exposed(
                defs.clone(),
                &settings,
                Some(1_000),
                &BTreeSet::new(),
                est(),
            );
            assert!(!hidden.is_hidden("save_note"), "{profile:?}");
            assert!(hidden.is_operator_hidden("save_note"), "{profile:?}");
            assert_eq!(
                hidden.promote("save_note"),
                Promotion::OperatorHidden,
                "{profile:?}"
            );
        }
    }

    /// #2950 review 2 (P2): the ceiling is the policy, not the stored
    /// definitions. A tool an earlier stage (Plan's disposition filter)
    /// already removed from the catalog is still operator-hidden to discovery
    /// and promotion; without a policy nothing is.
    #[test]
    fn operator_policy_covers_tools_removed_before_selection() {
        let defs = json!([tool("read_file", "k"), tool("tool_search", "k")]);
        let settings = ExposureSettings {
            read_only: true,
            ..Default::default()
        };
        let (_, hidden) = select_exposed(defs, &settings, None, &BTreeSet::new(), est());
        assert!(hidden.is_operator_hidden("write_file"));
        assert!(!hidden.is_operator_hidden("read_file"));
        assert!(
            !HiddenTools::default().is_operator_hidden("write_file"),
            "no policy, no ceiling"
        );
    }

    /// #2950 review 1: `read_only` narrows authority, not only visibility.
    #[test]
    fn read_only_attenuates_fs_write_and_only_narrows() {
        use crate::caveats::{Caveats, CaveatsExt, Scope};
        let writable = Caveats::top();
        let narrowed = ExposureSettings {
            read_only: true,
            ..Default::default()
        }
        .attenuate(writable.clone());
        assert!(!narrowed.permits_fs_write("/w/x"), "writes are denied");
        assert!(narrowed.permits_fs_read("/w/x"), "reads are untouched");
        assert!(narrowed.permits_exec("cargo"), "exec is untouched");
        assert!(narrowed.leq(&writable), "only narrows");
        assert_eq!(
            ExposureSettings::default().attenuate(writable.clone()),
            writable,
            "read_only = false is the identity"
        );
        let already_fenced = Caveats {
            exec: Scope::none(),
            ..Caveats::top()
        };
        let still_fenced = ExposureSettings {
            read_only: true,
            ..Default::default()
        }
        .attenuate(already_fenced);
        assert!(
            !still_fenced.permits_exec("cargo"),
            "enabling the toggle never widens another axis"
        );
    }

    /// #2950 review 1: the identity prompt names only operator-visible tools.
    #[test]
    fn identity_prompt_names_only_operator_visible_tools() {
        let soul = crate::DEFAULT_SOUL;
        assert_eq!(
            ExposureSettings::default().render_identity(soul),
            soul,
            "no operator filter → identity unchanged"
        );

        let read_only = ExposureSettings {
            read_only: true,
            ..Default::default()
        };
        let rendered = read_only.render_identity(soul);
        for hidden in WRITE_TOOL_NAMES {
            assert!(
                !mentions_tool(&rendered, hidden),
                "read_only prompt must not name `{hidden}`:\n{rendered}"
            );
        }
        assert!(mentions_tool(&rendered, "read_file"));
        assert!(rendered.starts_with("You are newt"));

        let four = ExposureSettings {
            allow_list: Some(
                ["read_file", "write_file", "edit_file", "run_command"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..Default::default()
        };
        let rendered = four.render_identity(soul);
        let line = rendered
            .lines()
            .find(|l| l.contains(TOOL_LINE_PREFIX))
            .expect("tool line survives");
        assert!(
            line.contains("You have tools: run_command, read_file, write_file, edit_file."),
            "{line}"
        );
        assert!(!mentions_tool(&rendered, "render_report"));
        assert!(!mentions_tool(&rendered, "list_dir"));
    }

    /// #2933: the registry renders the default soul exactly as the text filter did,
    /// for every operator filter the prompt can meet.
    #[test]
    fn registry_soul_matches_the_text_filter_under_every_filter() {
        let soul = crate::DEFAULT_SOUL;
        let filters = [
            ExposureSettings::default(),
            ExposureSettings {
                read_only: true,
                ..Default::default()
            },
            ExposureSettings {
                allow_list: Some(
                    ["read_file", "write_file", "edit_file", "run_command"]
                        .map(String::from)
                        .to_vec(),
                ),
                ..Default::default()
            },
            ExposureSettings {
                allow_list: Some(vec!["read_file".to_string()]),
                read_only: true,
                ..Default::default()
            },
            ExposureSettings {
                allow_list: Some(vec!["git".to_string()]),
                ..Default::default()
            },
        ];
        for settings in filters {
            assert_eq!(
                settings.render_identity(soul),
                settings.render_text_identity(soul),
                "{settings:?}"
            );
        }
    }

    /// #2933 ratchet: a component's `requires_tools` is exactly the built-in tools its
    /// text names, so the declaration cannot drift from the words the model reads.
    #[test]
    fn component_requirements_are_the_tools_the_text_names() {
        use crate::prompt_components::SOUL;
        let all = |_: &str| true;
        for c in SOUL.iter().filter(|c| c.id != "tools") {
            let text = crate::prompt_components::render_soul(&|x| x.id == c.id, &all);
            let named: Vec<&str> = super::super::catalog::ALL_TOOL_NAMES
                .iter()
                .copied()
                .filter(|t| mentions_tool(&text, t))
                .collect();
            let mut declared = c.requires_tools.to_vec();
            declared.sort_unstable();
            let mut named_sorted = named.clone();
            named_sorted.sort_unstable();
            assert_eq!(declared, named_sorted, "component `{}`", c.id);
        }
    }

    #[test]
    fn mentions_tool_matches_whole_words_only() {
        assert!(mentions_tool("call edit_file now", "edit_file"));
        assert!(!mentions_tool("call edit_files now", "edit_file"));
        assert!(!mentions_tool("use_edit_file", "edit_file"));
    }

    #[test]
    fn allow_list_hides_tools_not_in_list_even_in_full_profile() {
        // Regression for #2932: an explicit allow_list is the operator's final
        // say — tools not named are hidden regardless of profile or Kernel class.
        let defs = json!([
            tool("read_file", "k"),
            tool("write_file", "k"),
            tool("run_command", "k"),
            tool("tool_search", "k"),
            tool("git", "b"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Full,
            allow_list: Some(vec![
                "read_file".to_string(),
                "run_command".to_string(),
                "write_file".to_string(),
                "edit_file".to_string(),
            ]),
            ..Default::default()
        };
        let (exposed, hidden_tools) =
            select_exposed(defs, &settings, None, &BTreeSet::new(), est());
        let names: Vec<_> = exposed
            .as_array()
            .unwrap()
            .iter()
            .filter_map(entry_name)
            .collect();
        assert!(
            names.contains(&"read_file"),
            "allow-listed tool must appear"
        );
        assert!(
            names.contains(&"run_command"),
            "allow-listed tool must appear"
        );
        assert!(
            names.contains(&"write_file"),
            "allow-listed tool must appear"
        );
        assert!(
            !names.contains(&"tool_search"),
            "Kernel tool not in allow_list must be hidden"
        );
        assert!(
            !names.contains(&"git"),
            "ByIntent tool not in allow_list must be hidden"
        );
        assert!(
            hidden_tools.is_operator_hidden("tool_search"),
            "an operator-excluded Kernel tool is operator-hidden"
        );
        assert!(
            hidden_tools.is_operator_hidden("git"),
            "an operator-excluded ByIntent tool is operator-hidden"
        );
        assert!(
            !hidden_tools.is_hidden("git") && !hidden_tools.is_hidden("tool_search"),
            "operator-hidden tools are not in the discoverable (budget) pool"
        );
    }

    #[test]
    fn read_only_hides_write_tools_in_full_profile() {
        // Regression for #2932: read_only = true hides write_file, edit_file,
        // delete_file so the model never sees mutation tools.
        let defs = json!([
            tool("read_file", "k"),
            tool("write_file", "k"),
            tool("edit_file", "k"),
            tool("delete_file", "k"),
            tool("run_command", "k"),
            tool("list_dir", "k"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Full,
            read_only: true,
            ..Default::default()
        };
        let (exposed, hidden_tools) =
            select_exposed(defs, &settings, None, &BTreeSet::new(), est());
        let names: Vec<_> = exposed
            .as_array()
            .unwrap()
            .iter()
            .filter_map(entry_name)
            .collect();
        assert!(
            names.contains(&"read_file"),
            "read_file must remain visible"
        );
        assert!(
            names.contains(&"run_command"),
            "run_command must remain visible"
        );
        assert!(names.contains(&"list_dir"), "list_dir must remain visible");
        assert!(
            !names.contains(&"write_file"),
            "write_file must be hidden in read_only"
        );
        assert!(
            !names.contains(&"edit_file"),
            "edit_file must be hidden in read_only"
        );
        assert!(
            !names.contains(&"delete_file"),
            "delete_file must be hidden in read_only"
        );
        assert!(hidden_tools.is_operator_hidden("write_file"));
        assert!(hidden_tools.is_operator_hidden("edit_file"));
        assert!(
            !hidden_tools.is_hidden("write_file"),
            "a read_only exclusion is not a budget deferral"
        );
    }

    #[test]
    fn max_initial_tools_caps_additional_fill() {
        let defs = json!([
            tool("read_file", "k"),
            tool("git", "b"),
            tool("impact", "b"),
            tool("code_search", "b"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 100,
            max_initial_tools: 2,
            ..Default::default()
        };
        let plan = plan_exposure(&defs, &settings, Some(100_000), &BTreeSet::new(), est());
        // Kernel read_file is mandatory; cap of 2 total limits ByIntent fill.
        assert!(plan.exposed.contains(&"read_file".to_string()));
        assert!(plan.exposed.len() <= 2);
    }
}
