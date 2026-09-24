//! Context-window resolution for the active model: ONE derivation shared by
//! the turn loop, the startup memory budget, and the `/settings` Model section
//! (#2567), so what the operator is shown is what the loop enforces.
//!
//! Pure: the caller does the probing and cache I/O and passes the facts in.

/// Where the full context window came from, strongest declaration first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowSource {
    /// Reported by the server at adopt (`/props?model=`, `/v1/models`).
    Served,
    /// The capability cache's probed window (adopt got none).
    Cached,
    /// `[[model_tuning]] context_window`.
    Configured,
    /// A community tuning profile.
    Community,
}

/// Everything the resolution reads. Field names say where each value lives.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowFacts {
    pub(crate) kind: newt_core::BackendKind,
    /// The window the server declared at adopt (`inf_context_window`).
    pub(crate) served: Option<u32>,
    pub(crate) cached_window: Option<u32>,
    pub(crate) cached_hard_window: Option<u32>,
    pub(crate) cached_safe_context: Option<u32>,
    pub(crate) cached_max_ok_input: Option<u32>,
    pub(crate) configured_window: Option<u32>,
    pub(crate) community_window: Option<u32>,
    /// A numbered context-window 400 seen this session.
    pub(crate) recovered_window: Option<u32>,
    pub(crate) input_ceiling_pct: u32,
    /// `[[model_tuning]] num_ctx`, else the global `num_ctx`.
    pub(crate) configured_num_ctx: Option<u32>,
    /// `/context size <N>` for this session.
    pub(crate) context_size_override: Option<u32>,
}

/// The resolved window and the input limits derived from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextWindow {
    pub(crate) full_window: Option<u32>,
    pub(crate) window_source: Option<WindowSource>,
    /// The tightest window a server rejection has proven, if any.
    pub(crate) recovered_hard_window: Option<u32>,
    pub(crate) safe_context: Option<u32>,
    pub(crate) max_ok_input: Option<u32>,
    /// What the loop hands core as the window.
    pub(crate) num_ctx: Option<u32>,
}

/// The effective input ceiling percentage: `[context] input_ceiling_pct`, else
/// its declared default. The default lives in `ContextConfig`, not here.
pub(crate) fn input_ceiling_pct(cfg: &newt_core::Config) -> u32 {
    newt_core::config::normalize_input_ceiling_pct(cfg.context.as_ref().map_or_else(
        || newt_core::config::ContextConfig::default().input_ceiling_pct,
        |c| c.input_ceiling_pct,
    ))
}

/// Gather [`WindowFacts`] from the session's sources — the ONE construction
/// the turn loop, the startup memory budget and the `/settings` Inference
/// section share, so none of them can read a different field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn facts_for(
    cfg: &newt_core::Config,
    kind: newt_core::BackendKind,
    model: &str,
    served: Option<u32>,
    cached: &crate::probe::CapabilityEntry,
    community: &newt_core::tuning::CommunityTunings,
    recovered_window: Option<u32>,
    context_size_override: Option<u32>,
) -> WindowFacts {
    let tune = cfg.find_model_tuning(model);
    WindowFacts {
        kind,
        served,
        cached_window: cached.context_window,
        cached_hard_window: cached.hard_context_window,
        cached_safe_context: cached.safe_context,
        cached_max_ok_input: cached.max_ok_input,
        configured_window: tune.and_then(|t| t.context_window),
        community_window: community.find(model).and_then(|p| p.context_window),
        recovered_window,
        input_ceiling_pct: input_ceiling_pct(cfg),
        configured_num_ctx: tune.and_then(|t| t.num_ctx).or_else(|| crate::num_ctx(cfg)),
        context_size_override,
    }
}

pub(crate) fn resolve(f: WindowFacts) -> ContextWindow {
    let (requested_full_window, window_source) = selected_model_context_window(
        f.served.or(f.cached_window).map(|w| {
            let source = if f.served.is_some() {
                WindowSource::Served
            } else {
                WindowSource::Cached
            };
            (w, source)
        }),
        f.configured_window.map(|w| (w, WindowSource::Configured)),
        f.community_window.map(|w| (w, WindowSource::Community)),
    )
    .unzip();
    let recovered_hard_window =
        cap_context_window_by_recovery(f.recovered_window, f.cached_hard_window);
    let full_window = cap_context_window_by_recovery(requested_full_window, recovered_hard_window);
    let ceiling = |w| newt_core::config::input_percentage_ceiling(w, f.input_ceiling_pct);
    let safe_context = if f.kind == newt_core::BackendKind::Openai {
        full_window.map(ceiling).or(f.cached_safe_context)
    } else {
        recovered_hard_window
            .or(f.served)
            .map(ceiling)
            .or(f.cached_safe_context)
            .or(f.configured_window)
    };
    // `/context size <N>` caps both the safe context and the max-ok guard; a
    // raise past the probed value is honored (the operator opted in).
    let (safe_context, max_ok_input) = match f.context_size_override {
        Some(n) => (Some(n), Some(n)),
        None => (safe_context, f.cached_max_ok_input),
    };
    let requested_num_ctx = f
        .configured_num_ctx
        .or_else(|| context_window_for_core(f.kind, full_window, safe_context));
    ContextWindow {
        full_window,
        window_source,
        recovered_hard_window,
        safe_context,
        max_ok_input,
        num_ctx: cap_context_window_by_recovery(requested_num_ctx, recovered_hard_window),
    }
}

/// Preserve the unit boundary between a backend's full context window and an
/// already-derived input cap. OpenAI-compatible loops need the former so core
/// can reserve the active generation policy; Ollama keeps using the latter as
/// its conservative `num_ctx` KV-allocation fallback.
pub(crate) fn context_window_for_core(
    kind: newt_core::BackendKind,
    full_context_window: Option<u32>,
    safe_context: Option<u32>,
) -> Option<u32> {
    match kind {
        // Hosted APIs (OpenAI-compatible and Anthropic) get the full declared
        // window: core reserves the active generation policy itself.
        newt_core::BackendKind::Openai | newt_core::BackendKind::Anthropic => full_context_window,
        newt_core::BackendKind::Ollama | newt_core::BackendKind::Embedded => safe_context,
    }
}

/// Resolve the selected model's full window from strongest to weakest
/// declaration. The caller performs the exact model lookup for configured and
/// community profiles, so switching models naturally produces a new value.
pub(crate) fn selected_model_context_window<T>(
    live: Option<T>,
    configured: Option<T>,
    community: Option<T>,
) -> Option<T> {
    live.or(configured).or(community)
}

/// A numbered server rejection is an authoritative upper bound on later
/// turns. It may tighten an explicit/session window but never raise a tighter
/// operator choice. An ordinary discovered window is deliberately not passed
/// here, so experimental raises remain possible until the server rejects one.
pub(crate) fn cap_context_window_by_recovery(
    requested: Option<u32>,
    recovered_hard_window: Option<u32>,
) -> Option<u32> {
    match (requested, recovered_hard_window) {
        (Some(requested), Some(recovered)) => Some(requested.min(recovered)),
        (requested, recovered) => requested.or(recovered),
    }
}

#[cfg(test)]
#[path = "context_window_tests.rs"]
mod tests;
