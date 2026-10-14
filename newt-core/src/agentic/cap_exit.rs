//! Round-cap exit text: the nudge sent when the tool-round cap is hit, the
//! progress handoff, and the fallback reply when no summary arrives.
//!
//! Moved verbatim from `agentic/mod.rs`.

use super::{looks_like_unverified_stale_file_blocker, FinalReply};

/// Build the nudge appended to the message list when the tool-round cap is hit.
/// `progress` (the `<plan>`/`<state>` working memory, Step 27.5) is folded in so
/// the model summarizes against what it actually accomplished; `observed`
/// (#867 Part A) is the verified-paths manifest collected across the rounds.
pub(super) fn cap_exit_nudge(
    max_tool_rounds: usize,
    progress: Option<&str>,
    observed: &[String],
) -> String {
    // #867: the message list was just trimmed (`trim_for_summary`), so most
    // of the evidence this summary should cite is GONE — the forensic session
    // showed a model reconstructing plausible-but-nonexistent file paths from
    // its priors at exactly this point. Constrain the summary to what is
    // still verbatim in context; absence must be stated, not papered over.
    let mut nudge = format!(
        "You have reached the tool-round limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Give the operator a concise progress update \
         that separates completed and verified work, the current state or blocker, \
         and remaining work. The tool loop has stopped at the cap. Report whether \
         the operator's objective is complete or what remains; do not infer completion \
         merely because tools are disabled. Cite only file paths \
         that appear verbatim in the messages above — if the evidence you need \
         was in the omitted messages, say so plainly instead of reconstructing \
         file names or line numbers from memory. Do not narrate an intention to \
         keep working now (for example, \"let me read/edit/verify\"); list those \
         actions as remaining work and state that they have not run because the \
         round cap stopped further tool calls. Do not reproduce literal `<plan>` \
         or `<state>` tags from the working memory below. \
         Report an ACTION (an edit made, a test run or passing, a branch created, a \
         commit, a push, a PR opened) ONLY if a successful tool result above confirms \
         it — if you did not see the tool result, the action did not happen; list it \
         as remaining work instead. The workspace's real git state is checked against \
         this summary."
    );
    // #867 Part A: the ledger survived the trim — hand the model the REAL
    // manifest so grounded citation is possible, not just demanded.
    if !observed.is_empty() {
        nudge.push_str(
            "\n\nFile paths actually observed in tool results this run \
             (these exist — cite from this list):",
        );
        for p in observed {
            nudge.push_str("\n- ");
            nudge.push_str(p);
        }
    }
    if let Some(p) = progress {
        nudge.push_str(&format!("\n\nYour progress so far:\n{p}"));
    }
    nudge
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts, salvages the `<plan>`/`<state>`
/// progress so partial work survives (Step 27.5), and gives HONEST advice: a run
/// dominated by failed tool calls is a tooling/permissions problem, not too few
/// rounds, so we don't blindly tell the user to raise the cap.
pub(super) fn cap_exit_tokens_hint(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
) -> String {
    match accumulated {
        Some(u) => format!(
            " ({} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.input_tokens, u.output_tokens,
        ),
        None => String::new(),
    }
}

pub(super) fn cap_exit_advice(max_tool_rounds: usize, wasted_calls: usize) -> &'static str {
    // If at least one failed tool call per round, the cap was thrash, not a
    // genuine need for more rounds.
    if wasted_calls >= max_tool_rounds.max(1) {
        "most of those rounds were spent on tool calls that failed — the model \
         could not find a working edit/shell path, which is usually a tooling or \
         permissions issue rather than too few rounds; check `newt doctor`"
    } else {
        "increase the tool-round limit for the next attempt, or ask a more focused question"
    }
}

pub(super) fn cap_exit_progress_block(label: &str, progress: Option<&str>) -> String {
    match progress {
        Some(p) => {
            // The ledger is one step per line, but a bare newline inside a
            // Markdown paragraph is a soft break that the styled surface folds
            // into a space (`markdown/emitter.rs`, `Event::SoftBreak`). Two
            // trailing spaces are a CommonMark hard break: one step per
            // rendered line, and invisible on the plain surface, which prints
            // the bytes verbatim (`display::print_newt`).
            let body = format!("{label}:\n{}", humanize_cap_exit_progress(p));
            format!("\n\n{}", body.replace('\n', "  \n"))
        }
        None => String::new(),
    }
}

/// The plan and scratchpad are deliberately represented with lightweight XML
/// inside model context. Those tags are protocol framing, not user-facing UI;
/// deterministic cap fallbacks render the same saved content as ordinary text.
pub(super) fn humanize_cap_exit_progress(progress: &str) -> String {
    progress
        .replace("<plan>", "Plan:\n")
        .replace("</plan>", "")
        .replace("<state>", "Current state:\n")
        .replace("</state>", "")
        .trim()
        .to_string()
}

pub(super) fn cap_exit_fallback(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    wasted_calls: usize,
    progress: Option<&str>,
) -> FinalReply {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let advice = cap_exit_advice(max_tool_rounds, wasted_calls);
    let salvaged = cap_exit_progress_block("Progress captured before the summary failed", progress);
    FinalReply::harness(format!(
        "Paused at the tool-round limit ({max_tool_rounds} rounds){tokens_hint}. \
         The final summarization request also failed while preparing a progress \
         update; {advice}.{salvaged}"
    ))
}

/// Preserve the model-authored progress update while making the objective state
/// unambiguous. A pending-action classification is useful at a cap (it often
/// carries the best remaining-work handoff); the footer prevents those future
/// actions from being mistaken for completed work.
pub(super) fn cap_exit_progress_handoff(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    content: &str,
    has_pending_actions: bool,
    progress: Option<&str>,
) -> FinalReply {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let pending = if has_pending_actions {
        " The proposed next actions in this update have not run yet."
    } else {
        ""
    };
    let captured = cap_exit_progress_block("Captured working state", progress);
    FinalReply::from(content.to_owned()).with_notice(format!(
        "The tool loop reached the tool-round limit ({max_tool_rounds} rounds){tokens_hint}. \
         This progress handoff preserves the model's update.{pending}{captured}"
    ))
}

pub(super) fn cap_exit_summary_is_action_handoff(content: &str) -> bool {
    crate::NudgeClassifier::load_default()
        .classify(content)
        .is_pending_action()
        || looks_like_unverified_stale_file_blocker(content)
}

/// Leave a clean, completed model summary untouched. Only add the deterministic
/// handoff wrapper when the model still proposes work or core has structured
/// plan/state to preserve. Interactive callers use `TurnEndReason::RoundCap` to
/// add their own resume affordance independently of this textual shape.
pub(super) fn cap_exit_model_reply(
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
    content: &str,
    progress: Option<&str>,
) -> FinalReply {
    let pending = cap_exit_summary_is_action_handoff(content);
    if pending || progress.is_some() {
        cap_exit_progress_handoff(max_tool_rounds, accumulated, content, pending, progress)
    } else {
        FinalReply::from(content.to_string())
    }
}
