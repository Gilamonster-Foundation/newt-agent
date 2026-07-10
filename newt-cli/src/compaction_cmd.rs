//! `newt compaction` — diagnose the mid-loop context-compaction (summarizer)
//! configuration and *tune* it. It reads the resolved config + effective
//! summarizer state and reports the **effective** trim trigger (after the
//! `max_tool_rounds - 3` clamp), whether the trigger is count- or token-based,
//! and the summarizer backend — with warnings for the two failure modes that
//! bite weak local models:
//!   1. over-aggressive firing (a small `max_tool_rounds` clamps the threshold
//!      down, so compaction trims almost every multi-step turn); and
//!   2. a stalled summarizer that hangs the turn — there is no time-based abort,
//!      only a human Esc (#979).
//!
//! It is a pure, offline config diagnosis (no backend connection), like
//! `newt config`.

use crate::summarizer_cmd::{resolve_status, EffectiveSummarizer};
use newt_core::Config;
use std::path::Path;

/// The effective mid-loop trim COUNT trigger: the configured
/// `[tui].mid_loop_trim_threshold` clamped down to `max_tool_rounds - 3` (the
/// clamp newt-tui applies). Pure — unit-tested below. This is the lever behind
/// "the summarizer is too aggressive": a small `max_tool_rounds` clamps the
/// trigger down so compaction fires after only a handful of messages.
fn effective_count_trigger(configured_threshold: usize, max_tool_rounds: usize) -> usize {
    configured_threshold.min(max_tool_rounds.saturating_sub(3))
}

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let cfg = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };

    let provider = cfg
        .memory
        .as_ref()
        .map(|m| format!("{:?}", m.provider))
        .unwrap_or_else(|| "RollingWindow (default)".to_string());
    let tui = cfg.tui.unwrap_or_default();

    let max_rounds = tui.max_tool_rounds;
    let configured_thr = tui.mid_loop_trim_threshold;
    // The clamp the TUI applies (newt-tui mid_loop_trim_threshold): the count
    // trigger can never exceed max_tool_rounds - 3.
    let eff_count_thr = effective_count_trigger(configured_thr, max_rounds);
    let token_trigger = tui.mid_loop_trim_tokens.filter(|&t| t > 0);

    println!("Compaction (mid-loop context trim) — resolved config\n");
    println!("  memory provider               : {provider}");
    println!("  [tui] max_tool_rounds         : {max_rounds}");
    println!(
        "  [tui] mid_loop_trim_threshold : {configured_thr}   (message-count trigger, pre-clamp)"
    );
    println!("  -> effective count trigger    : {eff_count_thr}   = min({configured_thr}, max_tool_rounds - 3)");
    if eff_count_thr <= 5 {
        println!("       WARN: compaction fires after only {eff_count_thr} message(s) — very aggressive.");
        println!(
            "             It will trim almost every multi-step turn regardless of token budget."
        );
        println!(
            "             Lever: raise [tui].max_tool_rounds (the clamp is min(threshold, N-3))."
        );
    }
    match token_trigger {
        Some(t) => {
            println!("  [tui] mid_loop_trim_tokens    : {t}   (token-pressure trigger, active)");
        }
        None => {
            println!("  [tui] mid_loop_trim_tokens    : none   (token trigger DISABLED)");
            println!(
                "       WARN: compaction is COUNT-only — it can fire under a huge token budget."
            );
            println!("             Set [tui].mid_loop_trim_tokens to add a token-pressure gate.");
        }
    }

    // Summarizer (runs when compaction fires). Use the same effective-backend
    // logic as the runtime so this diagnosis matches what a real session does.
    let status = resolve_status()?;
    let embedded = matches!(
        &status.effective,
        EffectiveSummarizer::DefaultEmbedded { .. } | EffectiveSummarizer::OverrideEmbedded { .. }
    );

    println!("\nSummarizer (runs when compaction fires)");
    println!(
        "  config path                   : {}",
        status.config_path.display()
    );
    match &status.effective {
        EffectiveSummarizer::DefaultEmbedded { model, model_path } => {
            println!("  backend                       : embedded default (on-host CPU)");
            println!("  model / model_path            : {model} @ {model_path}");
            if status.config_exists && !status.backend_override {
                println!("  config file                   : present (knobs only; backend still defaults)");
            }
        }
        EffectiveSummarizer::DefaultDegradedSession { reason } => {
            println!("  backend                       : session model (degraded default)");
            println!("       WARN: {reason}");
        }
        EffectiveSummarizer::OverrideEmbedded { model, model_path } => {
            println!("  backend                       : embedded override");
            println!("  model / model_path            : {model} @ {model_path}");
        }
        EffectiveSummarizer::OverrideBackend {
            kind,
            model,
            endpoint,
        } => {
            println!(
                "  backend                       : {} {} @ {}",
                kind.map(|k| format!("{k:?}"))
                    .unwrap_or_else(|| "(session kind)".to_string()),
                model.as_deref().unwrap_or("(session model)"),
                endpoint.as_deref().unwrap_or("(session endpoint)"),
            );
            println!(
                "       WARN: explicit override can contend with the session model under load."
            );
        }
    }
    println!(
        "  timeout_secs / retries        : {} / {}",
        status.config.timeout_secs, status.config.retries
    );
    println!(
        "  fallback_model                : {}",
        status.config.fallback_model.as_deref().unwrap_or("none")
    );
    println!(
        "  keep_alive                    : {}",
        status
            .config
            .keep_alive
            .as_deref()
            .unwrap_or("(inherits session)")
    );
    println!("  overall abort on stall        : NONE");
    println!("       WARN: a stalled summarize has no time-based abort — only a human Esc stops");
    println!("             it; the turn hangs on \"compressing context...\" (see issue #979).");
    if embedded {
        println!(
            "             The embedded (candle) summarizer has NO per-request timeout at all."
        );
    }

    println!("\nTuning levers");
    println!(
        "  - Raise [tui].max_tool_rounds     : moves the count trigger up (min(threshold, N-3))."
    );
    println!("  - Set  [tui].mid_loop_trim_tokens : add a token-pressure gate (vs count-only).");
    println!(
        "  - Run  newt summarizer            : inspect or change backend, fallback, timeout, retries,"
    );
    println!("                                      keep_alive, or reset to the built-in default.");
    println!("\n  (Per-model [[model_tuning]] overrides of these knobs also apply when set.)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::effective_count_trigger;

    #[test]
    fn count_trigger_is_clamped_to_max_tool_rounds_minus_3() {
        // The clamp that bites weak local models: a small max_tool_rounds pulls
        // the trigger down so compaction fires almost immediately.
        assert_eq!(effective_count_trigger(40, 6), 3); // aggressive (the e2e repro)
        assert_eq!(effective_count_trigger(40, 40), 37); // shipped default — fine
        assert_eq!(effective_count_trigger(40, 25), 22); // the 2026-07-06 incident's 25-cap
                                                         // A lower configured threshold wins over the clamp.
        assert_eq!(effective_count_trigger(10, 40), 10);
        // Degenerate max_tool_rounds must not underflow (saturating_sub).
        assert_eq!(effective_count_trigger(40, 2), 0);
        assert_eq!(effective_count_trigger(40, 0), 0);
    }
}
