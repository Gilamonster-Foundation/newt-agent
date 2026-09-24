//! The `/settings` **Inference** section (#2567): what newt believes about the
//! active model and why — window, input ceiling, thinking, sampling, and the
//! server's own launch declaration — each value beside where it came from.
//!
//! Pure: the caller resolves every fact with the SAME functions the turn loop
//! uses (`context_window::resolve`, `preview_chat_generation`), so the section
//! cannot show a value the next request would not carry.

use newt_core::agentic::ChatGenerationPreview;
use newt_core::backend_probe::{LaunchProbe, LlamaCppLaunch};
use newt_core::metrics::fmt_count;
use newt_core::model_card::ChatCompletionsCapability;
use newt_core::role_profile::Cognition;

use crate::context_window::{ContextWindow, LimitSource, WindowSource};

pub(crate) struct Inference<'a> {
    pub(crate) model: &'a str,
    pub(crate) backend: &'a str,
    pub(crate) window: ContextWindow,
    pub(crate) input_ceiling_pct: u32,
    pub(crate) input_ceiling_pct_is_default: bool,
    /// The status bar's last `(used, budget)`.
    pub(crate) last_turn: Option<(u32, Option<u32>)>,
    pub(crate) cognition: Option<Cognition>,
    /// Where `cognition` came from, e.g. "session override", "active persona".
    pub(crate) cognition_source: &'a str,
    pub(crate) capability: ChatCompletionsCapability,
    pub(crate) card: Option<&'a str>,
    /// Why a configured card is switched off, when it is.
    pub(crate) card_inactive: Option<String>,
    pub(crate) preview: ChatGenerationPreview,
    /// What the launch-declaration probe found — a timeout or a refused key
    /// is reported as such, never as "not a router".
    pub(crate) launch: &'a LaunchProbe,
}

fn describe(source: WindowSource) -> &'static str {
    match source {
        WindowSource::Served => "reported by the server",
        WindowSource::Cached => "capability cache (probed earlier)",
        WindowSource::Configured => "[[model_tuning]] context_window",
        WindowSource::Community => "community tuning profile",
    }
}

/// Resolve the section from the live session, with the turn loop's own
/// resolvers. The launch fetch is the one network call; it is bounded and
/// happens before the panel opens, never in its draw loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gather(
    cfg: &newt_core::Config,
    choice: &crate::BackendChoice,
    model: &str,
    url: &str,
    api_key: Option<&str>,
    served: Option<u32>,
    cached: &crate::probe::CapabilityEntry,
    community: &newt_core::tuning::CommunityTunings,
    recovered_window: Option<u32>,
    context_size_override: Option<u32>,
    last_turn: Option<(u32, Option<u32>)>,
) -> Vec<String> {
    use newt_core::cognition::{
        cli_cognition, effective_cognition, persona_cognition, CognitionOverride,
    };
    let window = crate::context_window::resolve(crate::context_window::facts_for(
        cfg,
        choice.kind,
        model,
        served,
        cached,
        community,
        recovered_window,
        context_size_override,
    ));
    let input_ceiling_pct = crate::context_window::input_ceiling_pct(cfg);
    let cognition = effective_cognition();
    let cognition_source = match cli_cognition() {
        CognitionOverride::Set(_) | CognitionOverride::Off => "session override",
        CognitionOverride::Unset if persona_cognition().is_some() => "active persona",
        CognitionOverride::Unset => "unset",
    };
    let decision = choice.capability_decision();
    let capability = decision.chat_completions();
    let preview = newt_core::agentic::preview_chat_generation(
        cognition,
        cfg.find_model_tuning(model)
            .and_then(|t| t.output_allowance),
        capability,
        decision.reasoning_replay_scope(),
    );
    let launch = LaunchProbe::from_result(tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()?;
            newt_core::backend_probe::fetch_llamacpp_launch(&client, url, api_key, model).await
        })
    }));
    lines(&Inference {
        model,
        backend: &choice.name,
        window,
        input_ceiling_pct,
        input_ceiling_pct_is_default: input_ceiling_pct
            == newt_core::config::ContextConfig::default().input_ceiling_pct,
        last_turn,
        cognition,
        cognition_source,
        capability,
        card: choice.capabilities.card(),
        card_inactive: crate::applicability_prose(decision.applicability()),
        preview,
        launch: &launch,
    })
}

fn limit_source(source: LimitSource) -> &'static str {
    match source {
        LimitSource::PercentOfWindow => "a percentage of the window",
        LimitSource::Cached => "learned: capability cache",
        LimitSource::ConfiguredWindow => "[[model_tuning]] context_window, used as-is",
        LimitSource::SessionOverride => "/context size — this session's override",
    }
}

fn row(label: &str, value: &str, from: &str) -> String {
    format!("{label:<18} {value:<22} {from}")
}

fn count(n: Option<u32>) -> String {
    n.map_or_else(|| "unknown".to_string(), |n| fmt_count(u64::from(n)))
}

pub(crate) fn lines(i: &Inference<'_>) -> Vec<String> {
    let mut out = vec![
        format!("{} · backend {}", i.model, i.backend),
        String::new(),
    ];
    let w = &i.window;
    out.push(row(
        "context window",
        &count(w.full_window),
        w.window_source.map_or(
            "no declaration — set [[model_tuning]] context_window",
            describe,
        ),
    ));
    if let Some(hard) = w.recovered_hard_window {
        out.push(row(
            "  capped at",
            &count(Some(hard)),
            "a server rejection proved it",
        ));
    }
    let ceiling_from = match w.safe_context_source {
        Some(LimitSource::PercentOfWindow) => format!(
            "{}% of the window · [context] input_ceiling_pct{}",
            i.input_ceiling_pct,
            if i.input_ceiling_pct_is_default {
                " (default, #2565)"
            } else {
                ""
            }
        ),
        Some(source) => limit_source(source).to_string(),
        None => "no window to derive it from".to_string(),
    };
    out.push(row("input ceiling", &count(w.safe_context), &ceiling_from));
    if let (Some(max_ok), Some(source)) = (w.max_ok_input, w.max_ok_input_source) {
        let label = match source {
            LimitSource::SessionOverride => "input cap",
            _ => "largest accepted",
        };
        out.push(row(label, &count(Some(max_ok)), limit_source(source)));
    }
    if let Some((used, budget)) = i.last_turn {
        let value = format!("{} / {}", fmt_count(u64::from(used)), count(budget));
        out.push(row("last turn", &value, "tokens sent / send budget"));
    }
    out.push(String::new());
    out.extend(thinking_rows(i));
    out.push(String::new());
    out.extend(launch_rows(i));
    out
}

fn thinking_rows(i: &Inference<'_>) -> Vec<String> {
    let level = i.cognition.map(Cognition::label);
    let (value, why) = match i.preview.enable_thinking {
        Some(true) => (
            "ON".to_string(),
            format!(
                "sends enable_thinking=true · cognition {} ({})",
                level.unwrap_or("?"),
                i.cognition_source
            ),
        ),
        Some(false) => (
            "OFF (asked)".to_string(),
            format!(
                "sends enable_thinking=false · cognition {}",
                level.unwrap_or("?")
            ),
        ),
        // Nothing sent: the server's template default decides. Name the
        // first missing link (#2566) so the operator knows what to set.
        None => (
            "server default".to_string(),
            if let Some(inactive) = &i.card_inactive {
                inactive.clone()
            } else if i.card.is_none() {
                "not sent: no model card for this model".to_string()
            } else if i.capability.chat_template_kwargs != Some(true) {
                "not sent: card lacks chat_template_kwargs = true".to_string()
            } else if i.capability.cognition != Some(true) {
                "not sent: card lacks cognition = true".to_string()
            } else {
                "not sent: no cognition level — /settings cognition <level>".to_string()
            },
        ),
    };
    let mut rows = vec![row("model thinking", &value, &why)];
    let declared = match i.launch {
        LaunchProbe::Declared(launch) => Some(launch),
        _ => None,
    };
    if let Some(kwargs) = declared.and_then(|l| l.flag("--chat-template-kwargs")) {
        rows.push(row(
            "  server default",
            &key_values(kwargs),
            "router launch args",
        ));
    }
    let sampling = match (i.preview.temperature, i.preview.top_p) {
        (Some(t), Some(p)) => format!("temp {t} · top_p {p}"),
        _ => "server defaults".to_string(),
    };
    let sampling_from = match level {
        Some(level) if i.preview.temperature.is_some() => format!("cognition {level}"),
        _ => "not projected by this endpoint's card".to_string(),
    };
    rows.push(row("sampling", &sampling, &sampling_from));
    let max_output = match (i.preview.max_output_tokens, i.preview.output_allowance) {
        (Some(sent), _) => (fmt_count(u64::from(sent)), "sent as max_tokens".to_string()),
        (None, Some(reserved)) => (
            "not sent".to_string(),
            format!("{} reserved locally", fmt_count(u64::from(reserved))),
        ),
        (None, None) => ("not sent".to_string(), "server decides".to_string()),
    };
    rows.push(row("max output", &max_output.0, &max_output.1));
    rows
}

/// `{"enable_thinking": false}` as `enable_thinking=false`; anything that is
/// not a JSON object is shown verbatim.
fn key_values(json: &str) -> String {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) {
        Ok(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" · "),
        Err(_) => json.to_string(),
    }
}

fn launch_rows(i: &Inference<'_>) -> Vec<String> {
    let note = match i.launch {
        LaunchProbe::Declared(launch) => return declared_rows(launch),
        LaunchProbe::Absent => "the router declares no launch arguments for this model".to_string(),
        LaunchProbe::Unsupported => {
            "not a llama.cpp router (no /models launch declaration)".to_string()
        }
        LaunchProbe::Refused(status) => {
            format!("probe refused: HTTP {status} — check the backend's credential")
        }
        LaunchProbe::TimedOut => "probe timed out (3 s) — the server may be busy".to_string(),
        LaunchProbe::Failed(detail) => format!("probe failed: {detail}"),
    };
    vec![row("server launch", "—", &note)]
}

fn declared_rows(launch: &LlamaCppLaunch) -> Vec<String> {
    let mut rows = vec![row("server launch", "", "router /models status.args")];
    let mut args = launch.args.iter().peekable();
    while let Some(arg) = args.next() {
        let value = match args.peek() {
            Some(next) if arg.starts_with("--") && !next.starts_with("--") => args.next(),
            _ => None,
        };
        rows.push(
            format!("  {arg} {}", value.map_or("", String::as_str))
                .trim_end()
                .to_string(),
        );
    }
    rows
}

#[cfg(test)]
#[path = "inference_panel_tests.rs"]
mod tests;
