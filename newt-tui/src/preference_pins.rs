//! Conversation preference pins: what a conversation remembers about its
//! backend, and how a session restores or degrades it.
//!
//! `PreferenceBaseline` is the session's own starting posture;
//! `ConversationPreferenceSwitch` carries what a switch needs; `PinRestore` /
//! `PinDegraded` report what actually happened. The backend switch itself is
//! `super::refresh_backend` and the persona route is
//! [`crate::persona_route`] — this is the conversation-scoped layer over both.

use super::*;
// #2178 put this behind a crate-root re-export for exactly one caller:
// `restore_preference_pin`, which is in this module now. Naming it directly
// lets that re-export drop back to one name.
use crate::persona_route::persona_backend_route;

/// #1668: the posture this INVOCATION started with — the session's own
/// backend/model baseline plus the dial overrides its CLI flags installed,
/// snapshotted ONCE in `run_chat` after the flags land and before any
/// conversation pin is applied.
///
/// Every conversation switch resets to this before layering the incoming
/// conversation's own pin, which is what keeps posture *per conversation*: the
/// 2026-08-13 review (finding 2) showed that without a reset, applying one
/// conversation's pin left its backend and dials installed in the session
/// globals, and every conversation the session visited afterwards silently ran
/// — and, under the old ambient capture, was durably pinned — to it.
///
/// An axis a pin does not mention resolves to this baseline, so "unpinned"
/// means "whatever this invocation was launched with", never "whatever the
/// previous conversation left behind".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreferenceBaseline {
    /// `NEWT_PROVIDER` as the invocation resolved it (flag / loadout / sticky).
    pub provider: Option<String>,
    /// `NEWT_DGX_MODEL` as the invocation resolved it.
    pub model: Option<String>,
    /// The `/cognition` override the invocation started with.
    pub cognition: newt_core::cognition::CognitionOverride,
    /// The `/tenacity` override the invocation started with.
    pub tenacity: Option<newt_core::Tenacity>,
}

impl PreferenceBaseline {
    /// Snapshot the live dials beside the operator backend baseline the caller
    /// already resolved (`base_provider` / `base_model`).
    pub(crate) fn snapshot(provider: Option<String>, model: Option<String>) -> Self {
        Self {
            provider,
            model,
            cognition: newt_core::cognition::cli_cognition(),
            tenacity: newt_core::tenacity::cli_tenacity(),
        }
    }
}

/// Everything a conversation switch needs to re-seat the session posture
/// (#1668) — one record instead of a positional list, because these travel
/// together to all five switch sites (session-start resume, `/resume`,
/// `/conversation restore`/`new`, `/roadmap next`, `/new`).
pub(crate) struct ConversationPreferenceSwitch<'a> {
    /// The conversation store, or `None` in an ephemeral session (which has no
    /// pins at all — the switch then only resets to the baseline).
    pub store: Option<&'a newt_core::ConversationStore>,
    /// The conversation the session is switching TO.
    pub conversation_id: &'a str,
    /// The invocation baseline every unpinned axis resolves to.
    pub baseline: &'a PreferenceBaseline,
    /// The active persona, whose declared `backend:` outranks the baseline
    /// (but not the pin) for the backend axis.
    pub persona: Option<&'a Persona>,
    /// Posture actions still awaiting a durable row — dropped here, because
    /// they belonged to the conversation being left.
    pub pending: &'a mut newt_core::PreferenceActions,
    /// The operator backend baseline locals, reset to `baseline`. NEVER the
    /// pin: adopting a pin here is what let it propagate (review finding 2).
    pub base_provider: &'a mut Option<String>,
    pub base_model: &'a mut Option<String>,
    pub cfg: &'a newt_core::ResolvedConfig,
    pub choice: &'a mut BackendChoice,
    pub inf_url: &'a mut String,
    pub inf_model: &'a mut String,
    pub inf_kind: &'a mut newt_core::BackendKind,
    pub inf_key: &'a mut Option<String>,
    pub inf_context_window: &'a mut Option<u32>,
    pub color: bool,
    pub verbose: bool,
}

/// The result of applying a conversation's preference pin.
///
/// #1669 PR-A / ADR blocker 4: this used to be a bare `bool` (did the endpoint
/// move?), so a pin that could NOT be applied — a backend the config no longer
/// defines, a pin row that would not read — printed a notice and the session
/// carried on at baseline. That is exactly the silent-wrong-posture case the
/// ADR forbids: the tab says it is pinned to one backend and the next turn runs
/// somewhere else.
pub(crate) struct PinRestore {
    /// Whether the backend endpoint moved, so the caller re-probes telemetry.
    pub url_changed: bool,
    /// `Some` when the pin could not be fully established. The session is at a
    /// known baseline; the caller marks the tab degraded and refuses turns.
    pub degraded: Option<PinDegraded>,
}

/// A pin that could not be established, and enough to retry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinDegraded {
    /// Operator-facing reasons, verbatim from the apply plan.
    pub reasons: Vec<String>,
    /// The pin as stored — retained so a retry has something to retry once the
    /// operator fixes what was missing (usually a `[[backends]]` entry).
    pub pin: newt_core::OperatorPreferencePin,
}

impl PinDegraded {
    /// One line for the footer/list and for the turn refusal.
    pub fn summary(&self) -> String {
        format!("!pin — {}", self.reasons.join("; "))
    }
}

/// How this launch's conversation actually resolved — the input to "may the
/// startup preference pin apply?".
///
/// #1668 review-2 finding 6: this used to be a bare `resumed_at_start &&
/// !claim_refused` expression inline in `run_chat`, which no test could reach.
/// The test for the rule therefore hand-modelled the ordering by passing the
/// replacement id it had computed itself, so re-introducing the bug — applying
/// the HELD conversation's pin after a refused claim — would not have failed
/// it. Making the outcome a value lets `run_chat` and the test drive the same
/// gate, in [`apply_startup_preference_pin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupConversation {
    /// Minted fresh this launch — there is no stored pin to restore.
    Fresh,
    /// Resumed AND the claim was granted: this session holds the conversation.
    ResumedHeld,
    /// Resume refused (another live newt holds it); the session dropped onto a
    /// fresh replacement conversation instead.
    ResumedRefused,
}

impl StartupConversation {
    /// A pin applies only for a conversation this session actually HOLDS.
    ///
    /// A refused resume is the load-bearing case: the operator asked for a
    /// conversation someone else has open, so the session is now on a fresh
    /// replacement they never pinned. Applying the held one's posture there
    /// would push one operator's backend, model, and dials into another
    /// operator's session.
    pub(crate) fn applies_pin(self) -> bool {
        matches!(self, Self::ResumedHeld)
    }
}

/// The startup half of the pin restore: apply `sw` only when this session holds
/// the conversation the pin belongs to.
///
/// Exists so the gate is a seam rather than an inline conjunction — see
/// [`StartupConversation`]. It refuses on the outcome alone, so it is safe even
/// against a caller that passes the held conversation's id after a refusal.
pub(crate) fn apply_startup_preference_pin(
    outcome: StartupConversation,
    sw: ConversationPreferenceSwitch<'_>,
) -> bool {
    if !outcome.applies_pin() {
        return false;
    }
    restore_preference_pin(sw).url_changed
}

/// #1668: re-seat the session posture for the conversation being switched to —
/// the one step every conversation switch runs (session-start resume,
/// `/resume`, `/conversation restore`, `/roadmap next`, and `/new`).
///
/// Two halves, in order:
///
/// 1. **Reset to the invocation baseline.** The dials, the operator backend
///    baseline, and the routing env go back to what this process launched
///    with, so nothing the *previous* conversation's pin installed survives the
///    switch (review finding 2). A persona's declared `backend:` still routes:
///    it is session state, not the outgoing conversation's.
/// 2. **Apply the incoming conversation's own pin** through the SAME setters
///    the live commands use — `set_cli_cognition` / `set_cli_tenacity`, and the
///    `NEWT_PROVIDER` / `NEWT_DGX_MODEL` env pair + [`refresh_backend`] with
///    `/backends`' clear-the-model-override semantics — validated against
///    `cfg.backends` first, and skipping any axis this invocation's explicit
///    flags own ([`newt_core::runtime::cli_preference_axes`], review findings 4
///    and 9). An unknown pinned backend or an unparseable dial prints one line
///    and applies nothing for that axis; because capture is action-only, the
///    stored row survives that fail-open verbatim (review finding 3).
///
/// The pin is deliberately NOT adopted into `base_provider`/`base_model`: the
/// baseline answers "what does `/persona clear` revert to", which is a property
/// of the invocation and the operator's live choices, not of whichever
/// conversation is open.
///
/// Returns whether the endpoint URL changed, so the caller re-probes DGX
/// telemetry only when it matters (same contract as [`apply_persona_backend`]).
pub(crate) fn restore_preference_pin(sw: ConversationPreferenceSwitch<'_>) -> PinRestore {
    let ConversationPreferenceSwitch {
        store,
        conversation_id,
        baseline,
        persona,
        pending,
        base_provider,
        base_model,
        cfg,
        choice,
        inf_url,
        inf_model,
        inf_kind,
        inf_key,
        inf_context_window,
        color,
        verbose,
    } = sw;
    // Actions marked but not yet written belonged to the OUTGOING conversation
    // (it had no durable row); they must never land on the incoming one.
    *pending = newt_core::PreferenceActions::default();
    // Fail-open on a bad read: a corrupt pin must never block a resume, and
    // must never leave the session on the previous conversation's posture —
    // the baseline reset below still runs.
    let mut pin_read_failed = false;
    let pin = match store.map(|s| s.preference_pin(conversation_id)) {
        Some(Ok(Some(pin))) => pin,
        None | Some(Ok(None)) => newt_core::OperatorPreferencePin::default(),
        Some(Err(e)) => {
            print_newt(
                &format!(
                    "warning: could not read the conversation preference pin ({e}) — \
                     using this run's baseline preferences"
                ),
                color,
                verbose,
            );
            pin_read_failed = true;
            newt_core::OperatorPreferencePin::default()
        }
    };
    let owned = newt_core::runtime::cli_preference_axes();
    let configured: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
    let plan = pin.apply_plan(&configured, owned);
    for notice in &plan.notices {
        print_newt(notice, color, verbose);
    }
    // ADR blocker 4: a notice means the pin asked for something this process
    // could not establish. The baseline reset below still runs — so the session
    // lands somewhere KNOWN — but the caller must be told, because "at baseline
    // while claiming to be pinned" is a posture the operator did not choose.
    let mut degraded = (!plan.notices.is_empty() || pin_read_failed).then(|| PinDegraded {
        reasons: plan.notices.clone(),
        pin: pin.clone(),
    });
    if let Some(d) = degraded.as_mut() {
        if d.reasons.is_empty() {
            d.reasons
                .push("the stored preference pin could not be read".to_string());
        }
    }

    // ---- 1. reset the dials to the invocation baseline ----------------
    newt_core::cognition::set_cli_cognition(baseline.cognition);
    match baseline.tenacity {
        Some(t) => newt_core::tenacity::set_cli_tenacity(t),
        None => newt_core::tenacity::clear_cli_tenacity(),
    }
    *base_provider = baseline.provider.clone();
    *base_model = baseline.model.clone();

    // ---- 2. layer the pin's own axes over it --------------------------
    let mut applied: Vec<String> = Vec::new();
    if let Some(o) = plan.cognition {
        newt_core::cognition::set_cli_cognition(o);
        applied.push(format!(
            "cognition {}",
            pin.cognition.as_deref().unwrap_or("?")
        ));
    }
    if let Some(t) = plan.tenacity {
        newt_core::tenacity::set_cli_tenacity(t);
        applied.push(format!("tenacity {}", t.label()));
    }
    // The backend axis the session should route on once the switch settles:
    // the pin if it names one, else the active persona's declared route, else
    // the invocation baseline. Computed as a target and compared against the
    // live env, so an unchanged target costs no re-resolve (and no probe).
    let (mut provider, mut model) =
        match persona_backend_route(persona.map(|p| &p.profile), &configured) {
            // An unknown persona backend was already reported when the persona
            // activated; fall back to the baseline rather than repeat it here.
            Ok(Some((backend, model))) => (Some(backend), model),
            Ok(None) | Err(_) => (baseline.provider.clone(), baseline.model.clone()),
        };
    match plan.backend_axis {
        newt_core::BackendAxisAction::Leave => {}
        newt_core::BackendAxisAction::Route {
            provider: pinned,
            model: pinned_model,
        } => {
            applied.push(match &pinned_model {
                newt_core::RouteModel::Set(m) => format!("backend {pinned} (model {m})"),
                newt_core::RouteModel::Clear => format!("backend {pinned}"),
                // Say so out loud: the operator's own model survived a pinned
                // backend, which is the precedence rule doing its job.
                newt_core::RouteModel::Keep => {
                    format!("backend {pinned} (this run's model kept)")
                }
            });
            provider = Some(pinned);
            match pinned_model {
                // A backend pin that names a model installs it.
                newt_core::RouteModel::Set(m) => model = Some(m),
                // A backend pin with no model clears the override so the
                // backend's own default applies — the `/backends <name>` rule.
                newt_core::RouteModel::Clear => model = None,
                // This invocation owns the model axis: leave what the operator
                // supplied exactly as it is (review-2 finding 2).
                newt_core::RouteModel::Keep => {}
            }
        }
        newt_core::BackendAxisAction::ModelOnly(pinned_model) => {
            applied.push(format!("model {pinned_model}"));
            model = Some(pinned_model);
        }
    }
    let mut url_changed = false;
    let live = (
        std::env::var("NEWT_PROVIDER").ok(),
        std::env::var("NEWT_DGX_MODEL").ok(),
    );
    if live != (provider.clone(), model.clone()) {
        // One hold of the process-env lock for the pair — same discipline as
        // apply_persona_backend (#1850).
        {
            let _env = newt_core::process_env::lock();
            newt_core::process_env::set_or_remove("NEWT_PROVIDER", provider.as_deref());
            newt_core::process_env::set_or_remove("NEWT_DGX_MODEL", model.as_deref());
        }
        url_changed = refresh_backend(
            cfg,
            choice,
            inf_url,
            inf_model,
            inf_kind,
            inf_key,
            inf_context_window,
            color,
            verbose,
        );
    }
    // One line of operator visibility: silent dial changes would be invisible
    // until the next /psyche. Nothing prints when the pin applied nothing.
    if !applied.is_empty() {
        print_newt(
            &format!("session preferences restored: {}", applied.join(" · ")),
            color,
            verbose,
        );
    }
    // And one line when a pin was deliberately NOT applied because this run's
    // flags own the axis — otherwise the operator has no way to tell the pin
    // from the flag (review finding 9: the precedence must be visible).
    let suppressed = newt_core::PreferenceAxes {
        backend: owned.backend && pin.backend.is_some(),
        model: owned.model && pin.model.is_some(),
        cognition: owned.cognition && pin.cognition.is_some(),
        tenacity: owned.tenacity && pin.tenacity.is_some(),
    };
    if !suppressed.is_empty() {
        print_newt(
            &format!(
                "session preferences: this run's explicit {} beats the pin (kept for the next run)",
                suppressed.labels().join(" · ")
            ),
            color,
            verbose,
        );
    }
    PinRestore {
        url_changed,
        degraded,
    }
}
