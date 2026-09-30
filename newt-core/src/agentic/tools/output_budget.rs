use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

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
/// via `memory_fetch` with `{"address":"spill:<id>"}`) before the model-facing head/tail cap?
///
/// Pure so it can be unit-tested with an explicit `max_tokens` (the caller reads
/// the process-global). Spilling is only meaningful when `tool_offload` is on and
/// there is a budget (`max_tokens != 0`). Two independent triggers:
/// - **over model budget** — sized with [`cap_estimator`], the SAME conservative
///   ratio the cap uses, so anything the cap will truncate is spilled first (they
///   can never diverge and silently drop the elided middle).
/// - **over spill budget** — the raw output already exceeds
///   [`crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP`] chars.
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
    let over_spill_budget = out_chars > crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP;
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
    // Size the cap with the CONSERVATIVE ratio, not the 4 c/t context estimate:
    // dense output tokenizes denser, so a cap sized at 4 c/t admits more real
    // tokens than its budget. Using the conservative ratio for BOTH the
    // over-budget test and the char budget caps dense content sooner and tighter.
    let est = cap_estimator();
    if max_tokens == 0 || est.tokens_for_chars(text.len()) <= max_tokens {
        // A caller-selected view can be small while its retained source is
        // larger. An existing handle must remain discoverable in either case.
        return match spill_id {
            Some(id) => format!(
                "{text}\n\n{}",
                crate::agentic::content_spill::tool_output_retrieval_hint(id)
            ),
            None => text.to_string(),
        };
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
            "[… {elided} chars elided (head+tail shown). {} …]",
            crate::agentic::content_spill::tool_output_retrieval_hint(id)
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

/// Process-lifetime random key for the pagination MAC. Two UUID v4 values give
/// 32 bytes of OS randomness without adding a new dependency (uuid v4 is already
/// a dep and uses getrandom internally). The model never sees this key; it only
/// ever sees the 32-hex truncated tag in the footer.
fn page_mac_key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(a.as_bytes());
        key[16..].copy_from_slice(b.as_bytes());
        key
    })
}

/// The MAC input encoding's version. Bump this if the field order, framing,
/// or hash ever changes — a key is process-lifetime anyway (no tag survives
/// a restart), so this exists to make the encoding self-describing and
/// intentional, not to support cross-version verification.
const PAGE_MAC_VERSION: u8 = 0x01;

/// Compute the 32-hex MAC tag that authenticates a single pagination footer.
///
/// A keyed-BLAKE3 MAC over a documented, versioned, length-prefixed field
/// encoding — an authentication tag, not a content identity: it proves the
/// footer was minted by THIS process for exactly these inputs, under a
/// process-lifetime secret key the model never sees. It is not a
/// content-addressable id (those are public and reproducible by anyone;
/// this tag is reproducible only by code holding the key).
///
/// Encoding (all integers little-endian `u64`): version byte, then for each
/// variable-length field a length prefix followed by its bytes — no
/// separator byte is needed once lengths are explicit, unlike a fixed
/// delimiter, which a value containing that delimiter could exploit to
/// shift field boundaries.
fn page_mac_tag(
    path: &str,
    first_line: usize,
    char_offset_in: usize,
    body: &[u8],
    next_offset: usize,
    next_char_offset: usize,
) -> String {
    let mut h = blake3::Hasher::new_keyed(page_mac_key());
    h.update(&[PAGE_MAC_VERSION]);
    h.update(&(path.len() as u64).to_le_bytes());
    h.update(path.as_bytes());
    h.update(&(first_line as u64).to_le_bytes());
    h.update(&(char_offset_in as u64).to_le_bytes());
    h.update(&(body.len() as u64).to_le_bytes());
    h.update(body);
    h.update(&(next_offset as u64).to_le_bytes());
    h.update(&(next_char_offset as u64).to_le_bytes());
    let digest = h.finalize();
    digest.as_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Verify whether `tag` is the correct MAC for the given footer parameters.
/// Called by `prune.rs`; the key lives here, so verification is here too.
///
/// What this proves: `tag` could only have been minted by this process for
/// exactly this `(path, first_line, char_offset_in, body, next_offset,
/// next_char_offset)` tuple. What it does NOT prove: that the footer
/// belongs to the CURRENT `read_file` call rather than being a byte-for-byte
/// copy of an earlier real footer that ended up in the file's own source
/// content (e.g. a paginated page saved back to the same path and later
/// read WHOLE) — a replay reproduces every input this check sees, so it
/// verifies too. That specific replay is caught at the whole-file
/// passthrough call site in [`paginate_read_from`], which treats a
/// verifying tag on the content's OWN final line as reason to disambiguate
/// explicitly rather than as proof of current-call origin.
pub(crate) fn verify_page_tag(
    path: &str,
    first_line: usize,
    char_offset_in: usize,
    body: &str,
    next_offset: usize,
    next_char_offset: usize,
    tag: &str,
) -> bool {
    page_mac_tag(
        path,
        first_line,
        char_offset_in,
        body.as_bytes(),
        next_offset,
        next_char_offset,
    ) == tag
}

/// Window + cap a file's contents for `read_file`'s model-facing payload (#719,
/// #726). Returns lines `[offset, offset+limit)` (1-based `offset`, default 1;
/// `limit` default [`DEFAULT_READ_LIMIT`]), with the char backstop derived from
/// the shared token budget (`max_output_tokens` × chars/token — #726, replacing
/// #719's hardcoded 100k so both tools share one budget). A footer points at the
/// next window so the model paginates instead of drowning. A whole-file read
/// that fits both caps is returned verbatim (exact bytes). `max_output_tokens ==
/// 0` disables the char backstop (only the line window applies). Pure (no fs) —
/// unit-tested directly. Test-only: every production call site now threads an
/// explicit `char_offset` through [`paginate_read_from`] directly.
#[cfg(test)]
pub(super) fn paginate_read(
    contents: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    max_output_tokens: usize,
) -> String {
    paginate_read_from("", contents, offset, limit, max_output_tokens, None)
}

/// [`paginate_read`] with an optional `char_offset`: the character position
/// WITHIN the `offset` line to resume from. `offset` still picks the LINE;
/// `char_offset` narrows to a character position inside that one line. This
/// is how a single line longer than the char budget stays paginated under
/// the cap instead of either being cut with no exact resume point (the
/// pre-#2553 bug) or emitted whole, unbounded (round 1's bug, #2563: unbounded
/// with offload off, and an infinite re-spill loop with offload on, since the
/// whole-line page re-exceeds the spill cap every time). `char_offset` is
/// deliberately NOT part of `read_file`'s public schema (see
/// `tools/catalog.rs`) — the model learns it only from a page's own footer,
/// which is the one place it is ever correct to use.
///
/// `path` is included in the footer's MAC tag so `prune.rs` can verify that the
/// footer originated from this process's own producer and was not synthesised
/// from source content that happens to match the footer grammar.
pub(crate) fn paginate_read_from(
    path: &str,
    contents: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    max_output_tokens: usize,
    char_offset: Option<usize>,
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
    let char_offset = char_offset.unwrap_or(0);
    // Common case: a whole-file read that fits both caps → return verbatim.
    if char_offset == 0 && start == 1 && limit >= total && contents.len() <= max_chars {
        // #2638 review round 6 (P2): a genuine paginated page (body + its
        // real, MAC-verifying footer) saved back to THIS path and then read
        // WHOLE reproduces every MAC input, so the tag verifies even though
        // the bracketed line is now real source, not this call's metadata
        // (`verify_page_tag`'s doc comment names this exact replay). Append
        // a plain disambiguating line — NOT a footer (no `FOOTER_MARKER`
        // text, so `prune.rs` never strips it) — so the page stays source
        // and the summary counts every line, including the bracketed one.
        if crate::prune::final_line_is_a_verified_pagination_footer(contents, path) {
            return format!(
                "{contents}\n[end of file: the bracketed line above is part of the file]"
            );
        }
        return contents.to_string();
    }
    let start0 = start - 1;
    if start0 >= total {
        return format!("(offset {start} is past end of file — {total} lines total)");
    }
    let mut lines = contents.lines().skip(start0);
    let first_line_full = lines.next().expect("start0 < total, checked above");
    let first_line_chars = first_line_full.chars().count();
    // An out-of-range char_offset on the LAST line clamps to "" with no
    // footer (nothing after it to name), which is ambiguous to the model:
    // empty content, or a failure? Say so explicitly, mirroring the
    // "offset past end of file" message above.
    if char_offset > 0 && char_offset >= first_line_chars && start0 + 1 == total {
        return format!(
            "(char_offset {char_offset} is past the end of line {start}, which has \
             {first_line_chars} chars; call read_file with offset={} to continue)",
            start + 1
        );
    }
    // char_offset resumes mid-way through the `start` line only; every other
    // line in the window is taken in full.
    let first_line_body: String = if char_offset > 0 {
        first_line_full.chars().skip(char_offset).collect()
    } else {
        first_line_full.to_string()
    };
    let rest: Vec<&str> = lines.take(limit.saturating_sub(1)).collect();
    let end = start0 + 1 + rest.len(); // 1-based last line shown == end
    let mut body = first_line_body;
    for line in &rest {
        body.push('\n');
        body.push_str(line);
    }
    let char_capped = body.len() > max_chars;
    // The last line shown WHOLE, when the char cap cut on a line boundary.
    let mut whole_through = None;
    // Set when the cut lands mid-way through the `start` line itself, with no
    // earlier newline to land on: (line number, char position to resume from).
    let mut mid_line = None;
    if char_capped {
        let mut cut = max_chars;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        // Cut on the last whole line when there is one, so the footer can name
        // the exact line to resume from. A single line longer than the cap
        // (minified, base64, one huge log line) has no earlier boundary in
        // `body` — the cut lands inside the `start` line's own content, which
        // can only happen before any `rest` line's newline. Resume with
        // `char_offset` at the exact cut point instead of either losing the
        // remainder (pre-#2553) or emitting the whole line unbounded (#2563).
        match body[..cut].rfind('\n') {
            Some(newline) => {
                body.truncate(newline);
                whole_through = Some(start0 + body.lines().count());
            }
            None => {
                let emitted_chars = body[..cut].chars().count();
                let next_char_offset = char_offset + emitted_chars;
                body.truncate(cut);
                if next_char_offset >= first_line_chars {
                    // Cut landed exactly at the line's end (needs `rest`
                    // NON-empty — there is more file after this line for the
                    // footer to point at; when `rest` is empty the file is
                    // exhausted and there is nothing to resume). The whole
                    // `start` line WAS shown, so the last whole line shown is
                    // `start` itself (`start0 + 1`), not `start0` — using
                    // `start0` pointed the footer one line too early, back at
                    // this same line, looping forever when its length is an
                    // exact multiple of the cap.
                    whole_through = Some(start0 + 1);
                } else {
                    mid_line = Some((start, next_char_offset));
                }
            }
        }
    }
    let footer = if let Some((line, next_char_offset)) = mid_line {
        let tag = page_mac_tag(
            path,
            start,
            char_offset,
            body.as_bytes(),
            line,
            next_char_offset,
        );
        Some(format!(
            "payload truncated to {max_chars} chars (~{max_output_tokens} tokens); line {line} \
             continues: call read_file with offset={line} char_offset={next_char_offset} to \
             continue page={tag}"
        ))
    } else if let Some(last) = whole_through {
        let next = last + 1;
        let tag = page_mac_tag(path, start, char_offset, body.as_bytes(), next, 0);
        Some(format!(
            "payload truncated to {max_chars} chars (~{max_output_tokens} tokens) at line \
             {last} of {total}; call read_file with offset={next} to continue page={tag}"
        ))
    } else if end < total {
        let next = end + 1;
        let tag = page_mac_tag(path, start, char_offset, body.as_bytes(), next, 0);
        Some(format!(
            "showing lines {start}-{end} of {total}; \
             call read_file with offset={next} to continue page={tag}"
        ))
    } else {
        None
    };
    match footer {
        Some(f) => format!("{body}\n\n[{f}]"),
        None => body,
    }
}

/// The page `read_file` returns. With content offload on, a page over the
/// spill cap is replaced by a teaser + `spill:` handle — the model asked for
/// text and got a handle to redeem — so the page is held under that cap and
/// ends with the `offset=` to continue from instead.
pub(super) fn read_file_page(
    path: &str,
    contents: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    char_offset: Option<usize>,
    tool_offload: bool,
) -> String {
    if tool_offload {
        paginate_unspillable(path, contents, offset, limit, char_offset)
    } else {
        paginate_read_from(
            path,
            contents,
            offset,
            limit,
            max_output_tokens(),
            char_offset,
        )
    }
}

/// [`paginate_read`] held under the model-facing spill cap, for a payload read
/// back OUT of memory (a `spill:` address given to `read_file`). `read_file`'s
/// normal cap (~30k chars) exceeds the spill cap (16k), so without this the
/// answer to "read this spill" would itself be spilled — a fresh handle to the
/// very text the model asked for.
pub(super) fn paginate_unspillable(
    path: &str,
    contents: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    char_offset: Option<usize>,
) -> String {
    // paginate_read's continuation footer rides on top of its char cap.
    const FOOTER_HEADROOM: usize = 512;
    let unspillable = cap_estimator()
        .tokens_for_chars(crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP - FOOTER_HEADROOM);
    let tokens = match max_output_tokens() {
        0 => unspillable,
        budget => budget.min(unspillable),
    };
    paginate_read_from(path, contents, offset, limit, tokens, char_offset)
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
            out_chars < crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP,
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
        let big = crate::agentic::content_spill::TOOL_RESULT_SPILL_CAP + 1;
        assert!(should_spill_full_output(big, big, usize::MAX, true));
    }

    #[test]
    fn a_line_longer_than_the_page_budget_is_never_lost_across_pagination() {
        // Regression (#2553 review finding 2 AND #2563 round 2): a single line
        // longer than the char budget must stay reachable AND every page must
        // stay under the char cap. Round 1 fixed the reachability by emitting
        // the whole oversized line unbounded (a NEW bug, #2563: unbounded with
        // offload off, an infinite re-spill loop with offload on). The real
        // fix pages the oversized line too, via `char_offset` — the character
        // position within the named line to resume from — so following each
        // footer exactly (offset, and char_offset when present) reconstructs
        // every byte with no gap, no duplicate, and no page over budget.
        let long_line = "x".repeat(40_000);
        let original = format!("short one\n{long_line}\nshort two\nshort three\n");

        let budget_tokens = 5_000; // small budget forces the long line to be capped
        let max_chars = cap_estimator().chars_for_tokens(budget_tokens);
        let mut reconstructed = String::new();
        let mut offset = None;
        let mut char_offset = None;
        for _ in 0..20 {
            let page = paginate_read_from("", &original, offset, None, budget_tokens, char_offset);
            let (body, footer) = match page.rfind("\n\n[") {
                Some(marker_start) => (&page[..marker_start], Some(&page[marker_start..])),
                None => (page.as_str(), None),
            };
            assert!(
                page.len() <= max_chars + 300,
                "every page must stay under the char budget (~{max_chars} chars): {} bytes",
                page.len()
            );
            let mut next_offset = None;
            let mut next_char_offset = None;
            if let Some(footer) = footer {
                for tok in footer.split_whitespace() {
                    if let Some(v) = tok.strip_prefix("offset=") {
                        next_offset = v
                            .trim_end_matches(|c: char| !c.is_ascii_digit())
                            .parse()
                            .ok();
                    } else if let Some(v) = tok.strip_prefix("char_offset=") {
                        next_char_offset = v
                            .trim_end_matches(|c: char| !c.is_ascii_digit())
                            .parse()
                            .ok();
                    }
                }
            }
            // A page continuing MID-LINE (char_offset set) is glued directly
            // onto the previous page's body — it is the SAME line, not a new
            // one — everything else gets a newline separator.
            if char_offset.is_none() && !reconstructed.is_empty() {
                reconstructed.push('\n');
            }
            reconstructed.push_str(body);
            match next_offset {
                Some(n) => {
                    offset = Some(n);
                    char_offset = next_char_offset;
                }
                None => break,
            }
        }

        assert_eq!(
            reconstructed,
            original.trim_end_matches('\n'),
            "every byte of the original file must appear exactly once, in order"
        );
    }
}
