use super::*;

// ---- #719: read_file payload window/cap/pagination (pure, no fs) ----

#[test]
fn paginate_read_caps_a_large_file_to_the_default_window() {
    // A 15k-line file must NOT flood the model: default window is 2000 lines
    // with a footer to continue (regression for the 12.5k→168k saturation).
    let body: String = (1..=15_057)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = paginate_read(&body, None, None, DEFAULT_MAX_OUTPUT_TOKENS);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[1999], "line 2000");
    assert!(
        !out.contains("line 2001"),
        "window stops at 2000: {:?}",
        &out[..40]
    );
    assert!(out.contains("of 15057"), "footer names the total");
    assert!(
        out.contains("offset=2001"),
        "footer points at the next window"
    );
}

#[test]
fn paginate_read_offset_and_limit_return_just_that_window() {
    let body: String = (1..=100)
        .map(|n| format!("L{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = paginate_read(&body, Some(10), Some(5), DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(out.starts_with("L10\nL11\nL12\nL13\nL14"), "{out:?}");
    assert!(out.contains("offset=15"), "continues at line 15: {out:?}");
}

#[test]
fn paginate_read_small_file_is_returned_verbatim_without_a_footer() {
    // Whole-file read that fits both caps → exact bytes, no footer.
    assert_eq!(
        paginate_read("a\nb\nc\n", None, None, DEFAULT_MAX_OUTPUT_TOKENS),
        "a\nb\nc\n"
    );
}

#[test]
fn paginate_read_char_backstop_tracks_the_token_budget() {
    // #726: the char backstop is now token-derived (budget × chars/token),
    // NOT a hardcoded 100k. One enormous line: the line window can't help;
    // the token-derived char backstop must. With a 1000-token budget the
    // backstop is ~4000 chars, so a 50k-char line is truncated near there.
    let budget = 1_000;
    let max_chars = crate::tokens::TokenEstimation::default().chars_for_tokens(budget);
    let body = "x".repeat(50_000);
    let out = paginate_read(&body, None, None, budget);
    assert!(
        out.len() < max_chars + 300,
        "char-capped to the token budget (~{max_chars} chars): {} bytes",
        out.len()
    );
    assert!(out.contains("truncated"), "marks the truncation");
    assert!(
        out.contains("~1000 tokens"),
        "footer names the token budget: {out:?}"
    );

    // A LARGER budget keeps more of the same line — the backstop tracks the
    // budget rather than a fixed constant.
    let wide = paginate_read(&body, None, None, 4_000);
    assert!(
        wide.len() > out.len(),
        "a wider token budget keeps more chars: {} vs {}",
        wide.len(),
        out.len()
    );
}

#[test]
fn paginate_read_zero_budget_disables_the_char_backstop() {
    // #726: max_output_tokens == 0 means "no cap" — only the line window
    // applies, so a single huge line comes back verbatim.
    let body = "y".repeat(500_000);
    let out = paginate_read(&body, None, None, 0);
    assert_eq!(out, body, "zero budget = no char backstop");
}

#[test]
fn paginate_read_offset_past_end_is_a_clear_message() {
    let out = paginate_read("a\nb", Some(99), None, DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(out.contains("past end"), "{out:?}");
}

// ---- #726: shared token-based model-facing output cap ----

#[test]
fn cap_model_output_passes_small_output_through_unchanged() {
    // Well under budget → exact bytes, no marker.
    let small = "hello\nworld\n";
    assert_eq!(cap_model_output(small, DEFAULT_MAX_OUTPUT_TOKENS), small);
}

#[test]
fn cap_model_output_truncates_over_budget_as_head_tail() {
    let big = format!("HEAD_MARKER\n{}\nTAIL_MARKER", "middle\n".repeat(20_000));
    let out = cap_model_output_with_handle(&big, 1_000, 100, None);
    assert!(out.len() < big.len(), "must shrink: {} bytes", out.len());
    assert!(out.contains("HEAD_MARKER"), "head dropped: {out:?}");
    assert!(out.contains("TAIL_MARKER"), "tail dropped: {out:?}");
    assert!(out.contains("head+tail shown"), "marker present: {out:?}");
    assert!(
        !out.contains(&"middle\n".repeat(1_000)),
        "middle should be elided"
    );
}

#[test]
fn cap_model_output_truncates_at_a_char_boundary() {
    // A multi-byte char straddling the cut must not be split — the body must
    // stay valid UTF-8 (no panic, no replacement char).
    let budget = 10; // ~40 chars
    let body = "é".repeat(1_000); // 2 bytes each
    let out = cap_model_output(&body, budget);
    assert!(out.is_char_boundary(out.len()), "valid boundary");
    assert!(
        out.chars()
            .all(|c| c == 'é' || !c.is_control() || c == '\n'),
        "no split char: {out:?}"
    );
}

#[test]
fn cap_model_output_zero_budget_is_no_cap() {
    let body = "z".repeat(500_000);
    assert_eq!(cap_model_output(&body, 0), body);
}

#[test]
fn token_to_char_math_uses_the_default_four_chars_per_token() {
    // The context ESTIMATOR is the default 4 chars/token. NOTE: the output
    // CAP no longer sizes at this ratio — it uses the conservative
    // `output_cap_chars_per_token` (default 3, ~30k chars for a 10k budget)
    // so dense output can't overrun its token budget. See
    // `output_cap_sizes_at_the_conservative_ratio_not_the_estimate`.
    let est = crate::tokens::TokenEstimation::default();
    assert_eq!(est.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS), 40_000);
}

#[test]
fn output_cap_sizes_at_the_conservative_ratio_not_the_estimate() {
    // The conservative cap ratio (default 3) sizes the char backstop, so a
    // 10k-token budget caps at ~30k chars — not the estimator's 40k. This is
    // what keeps dense output (which tokenizes denser than 4 c/t) at/under
    // its real token budget.
    let cap = crate::tokens::TokenEstimation::new(DEFAULT_OUTPUT_CAP_CHARS_PER_TOKEN);
    assert_eq!(cap.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS), 30_000);
    assert!(
        cap.chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS)
            < crate::tokens::TokenEstimation::default().chars_for_tokens(DEFAULT_MAX_OUTPUT_TOKENS),
        "cap must be tighter than the estimate"
    );
}

#[test]
fn cap_model_output_caps_dense_body_the_estimate_would_pass() {
    // A body sized between the conservative cap (30k) and the estimator
    // backstop (40k): the old 4-c/t sizing would pass it VERBATIM; the
    // conservative 3-c/t sizing caps it. Relies on the default cap ratio (3)
    // — no global mutation (matches the max_output_tokens test convention).
    let body = "x".repeat(35_000);
    let out = cap_model_output(&body, DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(
        out.len() < body.len(),
        "conservative cap must truncate a 35k-char body at a 10k-token budget \
             (old 4-c/t backstop of 40k would have passed it); got {} bytes",
        out.len()
    );
    assert!(
        out.contains("head+tail shown"),
        "cap marker present: {out:?}"
    );
}

/// #898: the forge PR/MR-creation URL is extracted from git's push output
/// (GitHub and GitLab), and ordinary URLs do not false-positive.
#[test]
fn pr_creation_url_extracts_github_and_gitlab() {
    let github = "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                      remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n";
    assert_eq!(
        pr_creation_url(github),
        Some("https://github.com/OWNER/REPO/pull/new/fix/foo")
    );
    let gitlab = "remote: To create a merge request for topic, visit:\n\
                      remote:   https://gitlab.com/g/p/-/merge_requests/new?x=topic\n";
    assert_eq!(
        pr_creation_url(gitlab),
        Some("https://gitlab.com/g/p/-/merge_requests/new?x=topic")
    );
    // No PR URL present → None (ordinary fetch/clone output, plain links).
    assert_eq!(pr_creation_url("Already up to date.\n"), None);
    assert_eq!(
        pr_creation_url("see https://github.com/OWNER/REPO/issues/1"),
        None
    );
}

/// #898: after a push whose output carries a PR-creation URL,
/// `shell_envelope_output` appends the `gh pr create` next-step hint (and the
/// URL survives), while ordinary command output is left untouched.
#[test]
fn shell_envelope_output_appends_pr_hint_on_push() {
    let push = serde_json::json!({
        "exit_code": 0,
        "stdout": "",
        "stderr": "remote: Create a pull request for 'fix/foo' on GitHub by visiting:\n\
                   remote:      https://github.com/OWNER/REPO/pull/new/fix/foo\n",
    });
    let out = shell_envelope_output(&push, 50, false, false, None, None);
    assert!(out.contains("gh pr create --fill"), "hint missing: {out}");
    assert!(
        out.contains("https://github.com/OWNER/REPO/pull/new/fix/foo"),
        "url dropped: {out}"
    );

    // Ordinary output: no hint, payload unchanged.
    let plain = serde_json::json!({ "exit_code": 0, "stdout": "hello\n", "stderr": "" });
    let out = shell_envelope_output(&plain, 50, false, false, None, None);
    assert!(!out.contains("gh pr create"), "spurious hint: {out}");
    assert_eq!(out, "hello\n");
}

#[test]
fn shell_envelope_output_spills_full_output_before_head_tail_cap() {
    let full = format!(
        "HEAD_ONLY_MARKER\n{}\nMIDDLE_ONLY_MARKER\n{}\nTAIL_ONLY_MARKER\n",
        "alpha\n".repeat(10_000),
        "omega\n".repeat(10_000)
    );
    let envelope = serde_json::json!({
        "exit_code": 0,
        "stdout": full,
        "stderr": "",
    });
    let store = content_spill::SessionSpillStore::new([7u8; 16]);
    let mut display = ToolDisplay::new(Vec::new(), false, 80, 3, false);
    display.call("run_command", "large-output-command");
    let out = shell_envelope_output(&envelope, 50, false, true, Some(&store), Some(&mut display));
    display.result(&out);

    assert!(out.contains("HEAD_ONLY_MARKER"), "head dropped: {out}");
    assert!(out.contains("TAIL_ONLY_MARKER"), "tail dropped: {out}");
    // The teaser now names a `spill:<cid>` content handle (not a literal s0); it
    // must parse as a canonical CID and resolve in the store to the full payload.
    let handle = out
        .split("spill:")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("teaser names a spill handle");
    let cid = content_spill::SpillCid::parse(handle).expect("handle is a canonical CID");
    assert!(
        out.contains("grep=\"<pattern>\""),
        "search affordance missing: {out}"
    );
    let stored = store.fetch(&cid).expect("full output stored").redacted_text;
    assert!(
        stored.contains("MIDDLE_ONLY_MARKER"),
        "spilled payload was capped before storage"
    );
    assert!(stored.ends_with("TAIL_ONLY_MARKER\n"));
    let rendered = String::from_utf8(display.into_inner()).unwrap();
    assert!(
        rendered.contains("▓ TAIL_ONLY_MARKER\n…\n"),
        "operator spill lost the raw shell tail: {rendered}"
    );
    assert!(
        !rendered.contains("memory_fetch(\"spill:"),
        "operator saw the model teaser instead of raw shell output: {rendered}"
    );
}

#[test]
fn shell_envelope_without_streams_commits_the_exit_result() {
    let envelope = serde_json::json!({
        "exit_code": 3,
        "stdout": "",
        "stderr": "",
    });
    let mut display = ToolDisplay::new(Vec::new(), false, 80, 3, false);
    display.call("run_command", "exit 3");
    let out = shell_envelope_output(&envelope, 50, false, false, None, Some(&mut display));
    display.result(&out);

    // #1969: a NONZERO exit is now marked as a failure, because the ledger's
    // `ok` bit is `tool_result_ok`'s prefix test and `(exit 3)` read as a
    // success. The bare `(exit N)` rendering survives for exit 0, which is
    // the case it was written for.
    assert_eq!(out, "error: command exited 3");
    assert_eq!(
        String::from_utf8(display.into_inner()).unwrap(),
        "⚙  run_command: exit 3\n▒ error: command exited 3\n…\n"
    );
}
