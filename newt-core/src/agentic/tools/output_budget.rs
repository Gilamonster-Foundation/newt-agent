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

/// Process-wide model-facing output budget, in tokens. Defaults to
/// [`DEFAULT_MAX_OUTPUT_TOKENS`]; the resolved `[tools] max_output_tokens`
/// config value is pushed here once at the config-resolution entry
/// (`Config::resolve`) so the tool loop never re-reads config from disk. This is
/// the v1 (three-Cs "working code first") seam: a const default with the config
/// override wired at the entry, rather than threading a new `usize` through
/// `ChatCtx` + `execute_tool` + every call site (≈60, mostly tests). Follow-up:
/// thread it per-session like `tool_output_lines` once warranted.
static MAX_OUTPUT_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_OUTPUT_TOKENS);
static OUTPUT_HEAD_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_OUTPUT_HEAD_TOKENS);

/// Set the process-wide model-facing output budget (tokens). Called once from
/// `Config::resolve` with the resolved `[tools] max_output_tokens`. `0` means
/// "no cap" — see [`cap_model_output`] / [`paginate_read`].
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
    let est = crate::tokens::TokenEstimation::default();
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
        crate::tokens::TokenEstimation::default().chars_for_tokens(max_output_tokens)
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
