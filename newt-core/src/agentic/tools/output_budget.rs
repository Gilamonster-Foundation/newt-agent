use std::sync::atomic::{AtomicUsize, Ordering};

/// #719: default line window for `read_file`'s **model-facing** payload. The
/// on-screen display is capped separately; this bounds what enters the model's
/// context, so one read of a 15k-line file (e.g. `newt-tui/src/lib.rs`) can no
/// longer saturate a small local model's window and abandon the task.
const DEFAULT_READ_LIMIT: usize = 2_000;

/// #726: default token budget for any tool's **model-facing** payload, mirroring
/// Codex's `exec_command.max_output_tokens` (default 10k). One shared budget
/// caps BOTH `read_file` (via [`paginate_read`]'s char backstop) and
/// `run_command` (via [`cap_model_output`] around the shell envelope), so a
/// verbose command can no longer flood the window — the same failure mode #719
/// closed for `read_file`. Overridable by `[tools] max_output_tokens` in config;
/// see [`set_max_output_tokens`].
pub(super) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const DEFAULT_OUTPUT_HEAD_TOKENS: usize = 1_500;

/// Conservative chars/token used to SIZE the output cap (distinct from the 4
/// chars/token *estimate*). Dense tool output — hex dumps, base64, minified
/// JSON, columnar data — tokenizes far denser than the prose-derived 4 c/t
/// heuristic (observed ~3.3 c/t on Terminal-Bench `run_command` output), so a
/// "10k-token" cap sized at 4 c/t (40k chars) really admits ~12k+ real tokens
/// and can overrun a served window on its own. Sizing the cap at a conservative
/// 3 c/t (30k chars for a 10k budget) keeps a single capped result at/under its
/// token budget even for dense content — making a single-oversized-result
/// context overflow unrepresentable rather than something the loop must recover
/// from after the fact. Overridable by `[tools] output_cap_chars_per_token`.
pub(super) const DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN: usize = 3;

/// Process-wide model-facing output budget, in tokens. Defaults to
/// [`DEFAULT_MAX_OUTPUT_TOKENS`]; the resolved `[tools] max_output_tokens`
/// config value is pushed here at the runtime-application entry
/// (`Config::apply_runtime_settings`) so the tool loop never re-reads config
/// from disk. This is
/// the v1 (three-Cs "working code first") seam: a const default with the config
/// override wired at the entry, rather than threading a new `usize` through
/// `ChatCtx` + `execute_tool` + every call site (≈60, mostly tests). Follow-up:
/// thread it per-session like `tool_output_lines` once warranted.
static MAX_OUTPUT_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_OUTPUT_TOKENS);
static OUTPUT_HEAD_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_OUTPUT_HEAD_TOKENS);
static OUTPUT_CAP_CHARS_PER_TOKEN: AtomicUsize =
    AtomicUsize::new(DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN);

/// Set the process-wide model-facing output budget (tokens). Called from
/// `Config::apply_runtime_settings` with the resolved `[tools]
/// `max_output_tokens`. `0` means "no cap" — see [`cap_model_output`] /
/// [`paginate_read`].
pub fn set_max_output_tokens(max_tokens: usize) {
    MAX_OUTPUT_TOKENS.store(max_tokens, Ordering::Relaxed);
}

/// Set the head allocation for oversized `run_command` output. The tail gets
/// the remaining budget. `0` means pure-tail; values greater than the max output
/// budget are clamped by [`cap_model_output`].
pub fn set_output_head_tokens(head_tokens: usize) {
    OUTPUT_HEAD_TOKENS.store(head_tokens, Ordering::Relaxed);
}

/// The active model-facing output budget (tokens). [`DEFAULT_MAX_OUTPUT_TOKENS`]
/// until [`set_max_output_tokens`] overrides it.
pub(super) fn max_output_tokens() -> usize {
    MAX_OUTPUT_TOKENS.load(Ordering::Relaxed)
}

pub(super) fn output_head_tokens() -> usize {
    OUTPUT_HEAD_TOKENS.load(Ordering::Relaxed)
}

/// Set the conservative chars/token used to size the output cap. Called from
/// `Config::apply_runtime_settings` with `[tools]
/// `output_cap_chars_per_token`. Clamped to a minimum of 1 by
/// [`crate::tokens::TokenEstimation::new`] at the use site.
pub fn set_output_cap_chars_per_token(chars_per_token: usize) {
    OUTPUT_CAP_CHARS_PER_TOKEN.store(chars_per_token, Ordering::Relaxed);
}

/// The active conservative chars/token for cap sizing.
/// [`DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN`] until overridden.
pub(super) fn output_cap_chars_per_token() -> usize {
    OUTPUT_CAP_CHARS_PER_TOKEN.load(Ordering::Relaxed)
}

/// The [`crate::tokens::TokenEstimation`] used to SIZE the cap — the conservative
/// [`output_cap_chars_per_token`] ratio, NOT the 4 c/t context estimate. The
/// single owner of the cap ratio: `cap_model_output`, `paginate_read`, AND the
/// `run_command` spill gate all size from this, so the spill decision ("will the
/// cap truncate this?") can never diverge from what the cap actually does.
pub(super) fn cap_estimator() -> crate::tokens::TokenEstimation {
    crate::tokens::TokenEstimation::new(output_cap_chars_per_token())
}

/// Should a `run_command` result's FULL output be spilled (redacted → recoverable
/// via `memory_fetch("spill:<id>")`) before the model-facing head/tail cap?
///
/// Pure so it can be unit-tested with an explicit `max_tokens` (the caller reads
/// the process-global). Spilling is only meaningful when `tool_offload` is on and
/// there is a budget (`max_tokens != 0`). Two independent triggers:
/// - **over model budget** — sized with [`cap_estimator`], the SAME conservative
///   ratio the cap uses, so anything the cap will truncate is spilled first (they
///   can never diverge and silently drop the elided middle).
/// - **over spill budget** — the raw output already exceeds
///   [`crate::agentic::spill::TOOL_RESULT_SPILL_CAP`] chars.
pub(super) fn should_spill_full_output(
    out_bytes: usize,
    out_chars: usize,
    max_tokens: usize,
    tool_offload: bool,
) -> bool {
    if max_tokens == 0 || !tool_offload {
        return false;
    }
    let over_model_budget = cap_estimator().tokens_for_chars(out_bytes) > max_tokens;
    let over_spill_budget = out_chars > crate::agentic::spill::TOOL_RESULT_SPILL_CAP;
    over_model_budget || over_spill_budget
}

/// #726/#945: cap a tool's **model-facing** output to `max_tokens`' worth of
/// chars, estimated with the default chars/token heuristic
/// ([`crate::tokens::TokenEstimation`], 4 chars/token — the same constant the
/// context estimator uses). Oversized output is rendered as head+tail rather
/// than head-only so command summaries and failures at the end survive. A small
/// output (or `max_tokens == 0`, meaning no cap) passes through verbatim. Pure
/// (no fs / no global) — unit-tested directly.
pub(super) fn cap_model_output(text: &str, max_tokens: usize) -> String {
    cap_model_output_with_handle(text, max_tokens, output_head_tokens(), None)
}

pub(super) fn cap_model_output_with_handle(
    text: &str,
    max_tokens: usize,
    head_tokens: usize,
    spill_id: Option<&str>,
) -> String {
    if max_tokens == 0 {
        return text.to_string();
    }
    // Size the cap with the CONSERVATIVE ratio, not the 4 c/t context estimate:
    // dense output tokenizes denser, so a cap sized at 4 c/t admits more real
    // tokens than its budget. Using the conservative ratio for BOTH the
    // over-budget test and the char budget caps dense content sooner and tighter.
    let est = cap_estimator();
    if est.tokens_for_chars(text.len()) <= max_tokens {
        return text.to_string();
    }
    let max_chars = est.chars_for_tokens(max_tokens);
    let head_tokens = head_tokens.min(max_tokens);
    let head_chars = est.chars_for_tokens(head_tokens).min(max_chars);
    let tail_chars = max_chars.saturating_sub(head_chars);
    let total_chars = text.chars().count();
    let shown_chars = head_chars.saturating_add(tail_chars).min(total_chars);
    let elided = total_chars.saturating_sub(shown_chars);
    let head = take_chars(text, head_chars);
    let tail = take_tail_chars(text, tail_chars);
    let marker = match spill_id {
        Some(id) => format!(
            "[… {elided} chars elided (head+tail shown). Full output: \
             memory_fetch(\"spill:{id}\"); search it with \
             memory_fetch(\"spill:{id}\", grep=\"<pattern>\") …]"
        ),
        None => format!(
            "[… {elided} chars elided (head+tail shown; ~{max_tokens} token budget). \
             Narrow the command or use a more specific grep/filter if needed …]"
        ),
    };
    format!("{head}\n\n{marker}\n\n{tail}")
}

fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn take_tail_chars(text: &str, max_chars: usize) -> String {
    let mut chars: Vec<char> = text.chars().rev().take(max_chars).collect();
    chars.reverse();
    chars.into_iter().collect()
}

/// Window + cap a file's contents for `read_file`'s model-facing payload (#719,
/// #726). Returns lines `[offset, offset+limit)` (1-based `offset`, default 1;
/// `limit` default [`DEFAULT_READ_LIMIT`]), with the char backstop derived from
/// the shared token budget (`max_output_tokens` × chars/token — #726, replacing
/// #719's hardcoded 100k so both tools share one budget). A footer points at the
/// next window so the model paginates instead of drowning. A whole-file read
/// that fits both caps is returned verbatim (exact bytes). `max_output_tokens ==
/// 0` disables the char backstop (only the line window applies). Pure (no fs) —
/// unit-tested directly.
pub(super) fn paginate_read(
    contents: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    max_output_tokens: usize,
) -> String {
    let max_chars = if max_output_tokens == 0 {
        usize::MAX
    } else {
        // Conservative cap sizing (see `cap_estimator`): dense files (data,
        // base64, minified) tokenize denser than 4 c/t, so the char backstop
        // uses the conservative ratio to keep the model-facing payload under
        // its token budget.
        cap_estimator().chars_for_tokens(max_output_tokens)
    };
    let total = contents.lines().count();
    let start = offset.filter(|&o| o > 0).unwrap_or(1); // 1-based
    let limit = limit.filter(|&l| l > 0).unwrap_or(DEFAULT_READ_LIMIT);
    // Common case: a whole-file read that fits both caps → return verbatim.
    if start == 1 && limit >= total && contents.len() <= max_chars {
        return contents.to_string();
    }
    let start0 = start - 1;
    if start0 >= total {
        return format!("(offset {start} is past end of file — {total} lines total)");
    }
    let window: Vec<&str> = contents.lines().skip(start0).take(limit).collect();
    let end = start0 + window.len(); // 1-based last line shown == end
    let mut body = window.join("\n");
    let char_capped = body.len() > max_chars;
    if char_capped {
        let mut cut = max_chars;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let footer = if char_capped {
        Some(format!(
            "payload truncated to {max_chars} chars (~{max_output_tokens} tokens) from line \
             {start}; call read_file with a higher offset (and/or smaller limit) to continue"
        ))
    } else if end < total {
        Some(format!(
            "showing lines {start}-{end} of {total}; \
             call read_file with offset={} to continue",
            end + 1
        ))
    } else {
        None
    };
    match footer {
        Some(f) => format!("{body}\n\n[{f}]"),
        None => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_gate_uses_the_conservative_cap_ratio_not_the_estimate() {
        // Regression (cursor[bot] #1476): the spill gate must size with the SAME
        // conservative estimator as the cap. Output of 3_500 bytes at a
        // 1_000-token budget: the cap TRUNCATES it (3 c/t ⇒ ~1_167 > 1_000), so
        // it MUST be spilled. The old 4 c/t default under-counted (875 ≤ 1_000)
        // and skipped the spill, silently dropping the elided middle.
        let out_bytes = 3_500;
        let out_chars = 3_500; // ASCII ⇒ bytes == chars, and < TOOL_RESULT_SPILL_CAP
        assert!(
            out_chars < crate::agentic::spill::TOOL_RESULT_SPILL_CAP,
            "isolate over_model_budget: stay under the raw spill cap"
        );
        // The fix: conservative gate spills (over model budget).
        assert!(should_spill_full_output(out_bytes, out_chars, 1_000, true));
        // Guard the exact defect: the 4 c/t default would NOT have (the bug).
        assert!(crate::tokens::TokenEstimation::default().tokens_for_chars(out_bytes) <= 1_000);
        // And the conservative cap ratio DOES exceed the budget (so the cap cuts).
        assert!(cap_estimator().tokens_for_chars(out_bytes) > 1_000);
    }

    #[test]
    fn spill_gate_off_when_no_offload_or_no_budget() {
        // Even a huge output does not spill when offload is off or budget is 0.
        assert!(!should_spill_full_output(
            1_000_000, 1_000_000, 10_000, false
        ));
        assert!(!should_spill_full_output(1_000_000, 1_000_000, 0, true));
    }

    #[test]
    fn spill_gate_fires_on_raw_size_even_when_under_token_budget() {
        // The raw-size trigger is independent of the token budget: output past
        // TOOL_RESULT_SPILL_CAP spills even with a generous budget.
        let big = crate::agentic::spill::TOOL_RESULT_SPILL_CAP + 1;
        assert!(should_spill_full_output(big, big, usize::MAX, true));
    }
}
