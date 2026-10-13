//! Error classification and tool-round diagnostics for the agentic loop.
//!
//! Small predicates and nudges that read an error, an `AtomicBool` cancel
//! flag, or a tool result and decide what the loop should do: retry, bail,
//! dismiss a completed-spill viewport, print a synthetic tool result, or
//! announce activity. Pure and side-effect-free (aside from terminal output);
//! the loop threads in the same sinks it always did.
use super::display;
use super::tools;
use super::CompletedSpillRenderer;
use std::io;
/// True when a backend 400 says the model can't accept a `tools` field./// Ollama phrases it `"<model> does not support tools"`; OpenAI-compatible
/// servers vary, so we also accept the looser `"not support tools"`. Used to
/// drop tools and retry once, then keep them off for the turn (deepseek-r1).
pub(super) fn is_tools_unsupported_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("does not support tools") || s.contains("not support tools")
}

/// True when Ollama accepted the `tools` field but its internal XML tool-call
/// parser rejected the model's generated `<function>/<parameter>` markup before
/// returning an assistant message. This is a malformed generation, not proof
/// that the model lacks tool support, so retry with tools still advertised and
/// a corrective nudge instead of disabling tools for the whole turn.
pub(super) fn is_ollama_tool_xml_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("ollama") && s.contains("xml syntax error")
}

pub(super) fn ollama_tool_xml_retry_nudge() -> &'static str {
    "The previous assistant turn failed inside Ollama's XML tool-call parser. \
     Keep using tools, but emit exactly one valid native tool call with well-formed \
     arguments now. Do not answer in prose or wrap the call in explanatory XML."
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage, hallucination_count)`.
/// When `was_streamed` is true the text was already printed token-by-token.
///
/// Token-usage semantics (Step 18.1): `input_tokens` is the **largest single
/// prompt** the backend evaluated across the turn's rounds — the truthful
/// "how full did the context get" figure that feeds the capability ratchet —
/// NOT the per-round sum, which double-counts history (every round's prompt
/// re-includes all prior rounds; the B3 baseline measured 5.4× inflation).
/// `output_tokens` is the sum across rounds (each completion is new).
/// `true` once the caller's interrupt flag is set (Esc / Ctrl-C). Cheap relaxed
/// load — the flag is a one-way latch, so no ordering guarantees are needed.
pub(super) fn is_cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}

/// Take down any completed-spill viewport before the turn writes anything new.
///
/// The viewport's erase rewinds relative to the cursor, so it MUST come down
/// before the next canonical line (round dispatch → spinner detail / retry
/// indicator / streamed text, or the next tool header) lands below it and
/// breaks the rewind math. The committed excerpt above the frame is the
/// durable record, so dismissal loses nothing. Idempotent — an inner
/// recovery-loop retry re-calling this is a no-op.
pub(super) fn dismiss_completed_spill(
    renderer: &Option<std::sync::Arc<dyn CompletedSpillRenderer>>,
) {
    if let Some(renderer) = renderer {
        renderer.erase();
    }
}

/// Resolve as soon as the interrupt flag is set, polling at ~50 ms — fine for a
/// human keypress and cheap enough to lose the race to any real I/O future.
/// Never resolves when `cancel` is `None`, so a `select!` against it collapses
/// to just the other arm (no behavior change for headless callers).
pub(super) async fn cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) {
    match cancel {
        None => std::future::pending().await,
        Some(flag) => {
            // Poll briskly so an interrupt is felt promptly (well under the
            // ~100 ms human threshold) — the cost is negligible next to a model
            // round-trip.
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

/// Race a model future against the interrupt flag: `Some(v)` if it finished,
/// `None` if the user interrupted first (the future is dropped, cancelling any
/// in-flight request).
pub(super) async fn cancellable<F: std::future::Future>(
    cancel: Option<&std::sync::atomic::AtomicBool>,
    fut: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => None,
        v = fut => Some(v),
    }
}

pub(super) fn present_synthetic_tool_result<W: std::io::Write>(
    display: &mut display::ToolDisplay<W>,
    name: &str,
    args: &serde_json::Value,
    workspace: &std::path::Path,
    result: &str,
) {
    // Presentation-only (a completed result being re-rendered, not a live
    // dispatch decision) — the real authority was already enforced when
    // this call originally ran, so an unrestricted fence here costs
    // nothing and avoids threading `Caveats` through a display-only path.
    let (name, detail) =
        tools::tool_presentation(name, args, workspace, &crate::caveats::Scope::All);
    display.call(&name, &detail);
    display.result(result);
}

pub(super) fn print_synthetic_tool_result(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    result: &str,
    color: bool,
    completed_spill_renderer: &Option<std::sync::Arc<dyn CompletedSpillRenderer>>,
) {
    // A completed viewport from an earlier tool in the SAME batch may still
    // be painted; this synthetic block's header must not land below it (the
    // same erase-first discipline as the wired ToolDisplay::call). Dismissing
    // here rather than wiring the renderer in keeps synthetic results
    // static-only — an audit line does not need an interactive viewport.
    dismiss_completed_spill(completed_spill_renderer);
    let mut tool_display = display::ToolDisplay::new(
        io::stdout(),
        color,
        display::term_cols(),
        display::spill_lines(),
        display::spill_summary(),
    );
    present_synthetic_tool_result(
        &mut tool_display,
        name,
        args,
        std::path::Path::new(workspace),
        result,
    );
}

/// The legacy `animate` / `show_thinking` gate expressed as a [`LineCaps`]
/// override.
///
/// **Temporary, by design.** The real gate is `LineCaps::detect()`, which
/// requires two real terminals; `animate` is `color && thinking_stream_enabled()`
/// and never checked a TTY at all. Passing the legacy answer through the
/// arbiter's override seam keeps *when* the spinner appears bit-for-bit
/// identical while *how* it is drawn, ticked and erased moves onto the shared
/// implementation. Protocol mode still vetoes absolutely, so the override can
/// never re-open the JSON-RPC hazard.
///
/// Retiring this — so `thinking = "off"` informs the opt-out only and capability
/// is decided in one place — is the final step of the migration.
pub(super) fn legacy_caps(animate: bool) -> crate::tty::LineCaps {
    if animate {
        crate::tty::LineCaps::Own
    } else {
        crate::tty::LineCaps::None
    }
}

/// Announce that a validated tool call is about to run.
///
/// The one semantic tool-activity signal for every wire (chat-completions,
/// OpenAI, Anthropic, Responses): integrations learn what the agent is doing
/// from the agent, not by scraping rendered output. Free when nobody is
/// listening — the name is only allocated for a live observer.
pub(super) fn announce_tool_activity(name: &str) {
    if crate::lifecycle::observed() {
        crate::lifecycle::emit(crate::lifecycle::LifecycleEvent::ToolActivity {
            tool: name.to_string(),
        });
    }
}
