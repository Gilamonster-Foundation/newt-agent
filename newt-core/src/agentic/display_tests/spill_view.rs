use super::*;

/// #1433: the excerpt is capped by LOGICAL lines, so its "N rows" promise
/// only holds when the output happens to be narrow. One long line — a
/// compiler diagnostic, a minified JSON blob, a base64 payload — autowraps
/// past the budget and floods the scrollback.
///
/// codex names this rule explicitly (`exec_cell/render.rs`): "Wrap first so
/// that truncation is applied to on-screen lines rather than logical lines.
/// This ensures that a small number of very long lines cannot flood the
/// viewport."
///
/// Note the asymmetry this closes: the LIVE viewport already clips to width
/// (`docs/decisions/live_spill_viewport.md` §3). Only the committed excerpt
/// did not — so the same output was bounded while running and unbounded
/// once finished, which is backwards from what an operator expects.
#[test]
fn one_long_line_cannot_flood_the_row_budget() {
    const COLS: usize = 80;
    let long = "x".repeat(400);
    let out = spill_view_lines(&long, 3, COLS);

    let rendered_rows: usize = out.iter().map(|l| wrap_to_width(l, COLS).len()).sum();
    // header + 3 body rows + the trailing ellipsis
    assert!(
        rendered_rows <= 5,
        "a single {}-char line rendered {rendered_rows} rows at {COLS} columns \
         against a 3-row budget:\n{out:#?}",
        long.len()
    );
}

/// #1433: the budget is measured in rendered rows, but the TEXT is not
/// rewrapped. This excerpt is the canonical committed block, and
/// `plain_scroller_tui.md` names scrollback as "searchable, copy-pasteable,
/// and capturable with script/asciinema" — hard-wrapping would insert a
/// newline and a gutter mid-sentence, which looks identical to the
/// terminal's own soft-wrap but breaks copy-paste and breaks search across
/// the wrap point.
///
/// This is the regression that first showed up as
/// `artifact_read_central_display_never_echoes_recovered_body` failing: a
/// 103-character line split "…44 of 44 body / characters…" and a `contains`
/// assertion stopped matching. The assertion was right and the wrap was
/// wrong.
#[test]
fn a_line_wider_than_the_terminal_is_measured_but_not_rewrapped() {
    const COLS: usize = 40;
    let wide = "artifact:abc: returned 44 of 44 body characters at offset 0 (complete)";
    assert!(wide.len() > COLS, "the fixture must exceed the width");

    let out = spill_view_lines(&format!("first\n{wide}"), 3, COLS);
    let joined = out.join("\n");
    assert!(
        joined.contains("returned 44 of 44 body characters"),
        "the phrase was split across a wrap — copy-paste and search are \
         broken by rewrapping the canonical block:\n{joined}"
    );
}

/// #1433: measuring in rows must still SPEND the budget in rows — a wide
/// line costs what it actually occupies, so fewer lines are shown, not more
/// rows than asked for.
///
/// **#1973 declared amendment: this golden MOVED.** Pre-#1973 the wide
/// line alone consumed the whole 3-row budget, leaving zero room for
/// `a`/`b`/`c` — tail-only by construction never shows anything before
/// the tail item it kept. Post-#1973 the budget is split head+tail
/// (content_budget=2 → head=1/tail=1), so `a` (the head) now shows
/// alongside the wide line (the tail) — TWO body rows, not one. The
/// property this test still proves is unchanged and is what it is
/// actually named for: the wide line's row cost is still measured
/// correctly, so `b`/`c` are still excluded (only `a` fits the 1-row
/// head budget).
#[test]
fn a_wide_line_spends_the_row_budget_it_actually_occupies() {
    const COLS: usize = 20;
    // ~3 rows at width 18 (20 minus the "▒ " gutter).
    let wide = "w".repeat(50);
    let out = spill_view_lines(&format!("a\nb\nc\n{wide}"), 3, COLS);

    let body: Vec<&String> = out
        .iter()
        .filter(|l| l.starts_with('▒') || l.starts_with('▓'))
        .collect();
    assert_eq!(
        body,
        vec!["▒ a", &format!("▓ {wide}")],
        "the wide line's 3-row cost must still exclude b/c — only the \
         1-row head (a) and the wide tail fit the split budget:\n{out:#?}"
    );
    assert!(out.iter().any(|l| l.contains("lines hidden")), "{out:#?}");
    assert!(
        out.iter().any(|l| l.contains(SPILL_RECOVERY_HINT)),
        "every truncation marker names the way out:\n{out:#?}"
    );
}

/// #1235/#1973: the spill view shows BOTH ends with the issue's gutter
/// glyphs — small outputs show whole (▒ gutter + … end marker), overflow
/// shows a head and a tail with the ▲ hidden-count boundary between them
/// and the ▓ thumb on the true tail line. view=0 = unbounded raw
/// (historical behavior).
///
/// **#1973 declared amendment: the overflow golden MOVED**, from
/// tail-only (`l3,l4,l5`) to head+tail (`l1` .. `l5`) — see the module
/// doc on [`spill_view_lines`] for why tail-only is a defect, not a
/// style choice.
#[test]
fn spill_view_shows_both_ends_with_gutter_glyphs() {
    // Fits: whole output, ▒ gutter, end marker — unchanged by #1973.
    let small = spill_view_lines("a\nb\nc", 3, 80);
    assert_eq!(small, vec!["▒ a", "▒ b", "▒ c", "…"]);

    // Overflows: a head AND a tail (#1973 — neither end is fully
    // hidden), ▲ carries the hidden count, ▓ thumbs the true tail.
    let big = spill_view_lines("l1\nl2\nl3\nl4\nl5", 3, 80);
    assert_eq!(
        big,
        vec![
            "▒ l1",
            "▲ 3 lines hidden  [/spill N raises this view]",
            "▓ l5",
            "…"
        ]
    );

    // Unbounded: raw lines, no gutter.
    assert_eq!(spill_view_lines("x\ny", 0, 80), vec!["x", "y"]);
    // Empty: nothing.
    assert!(spill_view_lines("", 3, 80).is_empty());
}

/// **#1973 declared amendment: this golden MOVED** — same head+tail
/// reasoning as `spill_view_shows_both_ends_with_gutter_glyphs`.
#[test]
fn completed_tool_output_uses_the_spill_view() {
    let output = "l1\nl2\nl3\nl4\nl5";

    assert_eq!(
        spill_view_lines(output, 3, 80),
        vec![
            "▒ l1",
            "▲ 3 lines hidden  [/spill N raises this view]",
            "▓ l5",
            "…"
        ]
    );
    let raw: Vec<String> = output.lines().map(str::to_string).collect();
    assert_eq!(spill_view_lines(output, 0, 80), raw);
}

/// #1263: the COMPLETED excerpt names its real recovery path at the point
/// of use — it is plain printed text sharing the live viewport's glyphs, so
/// without the hint it masqueraded as the interactive scroller (the
/// diagnosed operator tried to expand it in scrollback and could not).
///
/// **#1973 declared amendment:** the hint no longer lives at a fixed
/// index — with a head shown before it, the boundary marker (and its
/// hint) sits wherever the head ends, not always at `lines[0]`. The
/// property this test proves — the hint appears somewhere, exactly
/// once, whenever truncation occurs — is unchanged.
#[test]
fn completed_excerpt_names_its_recovery_path() {
    let lines = spill_view_lines("l1\nl2\nl3\nl4\nl5", 3, 80);
    let hint_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("/spill N raises this view"))
        .collect();
    assert_eq!(
        hint_lines.len(),
        1,
        "the boundary marker must carry the recovery hint exactly once: {lines:?}"
    );
    // #1263 fingerprint pin (the other half lives in the spill_view tests):
    // the completed excerpt's last row is the INERT `…` — never the live
    // frame's ⧉/▣ boundary.
    assert_eq!(lines.last().map(String::as_str), Some("…"));
    // The fits-entirely form is inert-terminated too.
    let small = spill_view_lines("a\nb", 3, 80);
    assert_eq!(small.last().map(String::as_str), Some("…"));
}

/// #1973 anti-vacuous pair, replaying the incident's own shape: a script
/// prints real results, then something unrelated in cleanup crashes.
/// Tail-only showed ONLY the traceback — the confirming responses were
/// entirely inside the hidden middle. Both the LAST result line and the
/// TRACEBACK'S OWN HEAD must be visible in one default render.
///
/// `view=8` (not the bare `[tui] spill_lines` default of 3) is chosen
/// deliberately and stated so, not smuggled in: no fold of a 3-row
/// budget can show 2 result lines AND any part of a 12-line traceback —
/// there is not enough room at any split, head+tail or otherwise. 8 is
/// what "the default view" (as the issue frames it, contrasted with an
/// operator's `/spill N` AFTER already reading a misleading render)
/// looks like for a moderately verbose tool result; the property under
/// test is the SPLIT's behavior, not a specific numeric default.
#[test]
fn results_then_a_crash_shows_the_last_result_and_the_traceback_head() {
    let output = [
        "Response 0: initialize ok",
        "Response 1: tools/list ok",
        "Traceback (most recent call last):",
        "File \"cleanup.py\", line 40, in shutdown",
        "File \"cleanup.py\", line 30, in _terminate",
        "File \"cleanup.py\", line 20, in _kill",
        "File \"cleanup.py\", line 15, in _signal",
        "File \"cleanup.py\", line 10, in _reap",
        "File \"cleanup.py\", line 8, in _wait",
        "File \"cleanup.py\", line 6, in _proc",
        "File \"cleanup.py\", line 4, in _pid",
        "ProcessLookupError: [Errno 3] No such process",
    ]
    .join("\n");
    let output = output.as_str();
    let out = spill_view_lines(output, 8, 80);
    let joined = out.join("\n");

    assert!(
        joined.contains("Response 1: tools/list ok"),
        "the last result line — the confirming evidence — must not be \
         fully hidden inside the fold:\n{out:#?}"
    );
    assert!(
        joined.contains("Traceback (most recent call last):"),
        "the traceback's own head must not be fully hidden either — \
         showing only its tail (the pre-#1973 behavior) is exactly the \
         evidence-inversion this issue is about:\n{out:#?}"
    );
    // The true tail — the actual exception — is still the tail's last
    // line (▓-thumbed): the fix ADDS a head, it does not sacrifice the
    // tail cargo-style output already depended on.
    assert!(
        out.last().is_some_and(|l| l == "…"),
        "inert-terminated as always:\n{out:#?}"
    );
    assert!(
        joined.contains("ProcessLookupError"),
        "the actual exception must still be visible — this is not a \
         head-only regression of the tail:\n{out:#?}"
    );
}

/// #1973 anti-vacuous TWIN: cargo-style output (the decisive content
/// really is last) must not regress. Same `view` as the sibling test
/// above for a fair comparison.
#[test]
fn cargo_style_output_with_errors_last_still_shows_the_errors() {
    let output = [
        "Compiling foo v0.1.0",
        "Compiling bar v0.1.0",
        "Compiling baz v0.1.0",
        "Compiling qux v0.1.0",
        "Compiling quux v0.1.0",
        "Compiling corge v0.1.0",
        "Compiling grault v0.1.0",
        "error[E0499]: cannot borrow `x` as mutable more than once",
        "  --> src/lib.rs:42:5",
        "error[E0502]: cannot borrow `x` as immutable",
        "  --> src/lib.rs:43:5",
        "error: could not compile `foo` (bin \"foo\") due to 2 previous errors",
    ]
    .join("\n");
    let output = output.as_str();
    let out = spill_view_lines(output, 8, 80);
    let joined = out.join("\n");

    assert!(
        joined.contains("error: could not compile `foo` (bin \"foo\") due to 2 previous errors"),
        "the final cargo summary line — the decisive content for THIS \
         shape — must still be visible:\n{out:#?}"
    );
    assert!(
        joined.contains("E0499") || joined.contains("E0502"),
        "at least one of the actual diagnostics should still be in the \
         tail window:\n{out:#?}"
    );
}

/// #1973's small-render finding, checked directly: the reserve for the
/// boundary marker was previously 0 (an unaccounted extra row on top of
/// `view` content rows), so a `view`-row budget rendered `view + 2`
/// total rows (content + marker + the pre-existing trailing `…`), not
/// `view`. Reserving 1 row for the marker tightens this to `view + 1` —
/// the one row still unaccounted for is the trailing `…`, which predates
/// #1973 and is not part of "the marker's own height".
#[test]
fn the_marker_reserve_is_tightened_by_its_own_height() {
    for view in [2usize, 3, 5, 8, 12] {
        // Enough lines to guarantee overflow at every tested view.
        let lines: Vec<String> = (0..view + 20).map(|i| format!("l{i}")).collect();
        let out = spill_view_lines(&lines.join("\n"), view, 80);
        assert!(
            out.len() <= view + 1,
            "view={view}: rendered {} total rows, expected at most view+1 \
             ({}) — the marker's reserve regressed:\n{out:#?}",
            out.len(),
            view + 1
        );
    }
}
