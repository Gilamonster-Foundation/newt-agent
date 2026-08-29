//! **Newt Markup A0 anti-sprawl ratchet (#1823, epic #1803).**
//!
//! The A0 inventory (`docs/findings/2026-08-newt-markup-a0-inventory.md`)
//! baselined every duplicate interaction-machinery site the Newt Markup
//! migration must eventually delete. This test arms those baselines as a
//! migration TRIPWIRE: the per-file site counts below may only go DOWN.
//!
//! - A count above its baseline (or a hit in an unlisted file) means a new
//!   duplicate path was added mid-migration — delete it, or justify adjusting
//!   the table in PR review (epic law 10: a second permanent path is a
//!   regression).
//! - A count below its baseline is progress — decrement the table in the same
//!   PR that deletes the site, so the ratchet re-arms at the new floor.
//!
//! This is a migration tripwire, not a forever-architecture parser: the
//! detectors are deliberately narrow name/shape needles over PRODUCTION lines
//! (the shared scanner in `tests/common/mod.rs` skips `#[cfg(test)]` regions
//! by brace depth), and each category names its rationale. Two honesty notes
//! from the count-verification pass: (a) the interactive `grep` on the dev
//! box is a ugrep wrapper that produced FALSE NEGATIVES during baselining, so
//! this test scans files natively instead of shelling out; (b) a name-needle
//! registry detects deletions and same-name duplicates, never a NEWLY NAMED
//! parallel implementation — those are caught by review, with this inventory
//! as the checklist.

use std::collections::BTreeMap;
use std::path::Path;

mod common;
use common::{for_each_production_line, production_roots, workspace_root};

/// The shared scanner already excludes build output, hidden directories,
/// and parent-gated test children structurally; this ratchet adds nothing.
fn no_extra_skips(_: &Path) -> bool {
    false
}

/// One armed category. Counting runs over a file's **whitespace-squeezed**
/// production code, so a needle cannot be lost to a rustfmt line split
/// (`console\n    .ask(` and `console.ask(` are the same site) — the
/// dangerous direction, because a silent count DROP reads as "Progress!
/// Ratchet down" and lowers a baseline that should not move.
struct Category {
    name: &'static str,
    /// Sites in one file's squeezed production code.
    count: fn(&FileCode) -> usize,
    /// Workspace-relative path -> exact expected production count.
    baseline: &'static [(&'static str, usize)],
    rationale: &'static str,
}

/// One file's production code, squeezed of all whitespace, plus the facts a
/// category may need about the file as a whole.
struct FileCode {
    squeezed: String,
    /// The file names `pulldown_cmark` — the only `Parser` this repo's
    /// dialect cares about.
    imports_pulldown: bool,
    /// Hand-laid column rows, tallied per LINE from the raw source.
    ///
    /// Every other category reads `squeezed`, which comes from the shared
    /// scanner with **string-literal contents blanked** — `"{:<28} {:<16}"`
    /// arrives as `"________________"`. That is right for identifier needles
    /// and it makes a format-string shape unarmable through that field, which
    /// is why these sites stayed invisible. The scanner already passes the
    /// raw line as its third argument; nothing had used it.
    adhoc_sites: usize,
}

/// Non-overlapping occurrences of `needle` in `hay`, ignoring matches whose
/// preceding character continues an identifier (so `Question{` does not
/// match `PendingQuestion{`, and `console.ask(` does not match
/// `my_console.ask(` — a different receiver is a different site).
fn count_sites(hay: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        let boundary = at == 0
            || !hay[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary {
            count += 1;
        }
        from = at + needle.len();
    }
    count
}

fn count_any(hay: &str, needles: &[&str]) -> usize {
    needles.iter().map(|n| count_sites(hay, n)).sum()
}

/// `raw` with its trailing line comment removed, keeping literal contents.
///
/// The shared scanner hands back `code` — the same line with literals blanked
/// and the trailing comment truncated — and `strip_string_literals` replaces
/// each char with exactly one char, so `code`'s CHAR count is the length of
/// the raw line's non-comment prefix. Chars, not bytes: blanking turns a
/// multi-byte char into a one-byte `_`, so a byte index would slice a row
/// containing `…` or a box-drawing glyph in the wrong place.
/// `strip_string_literals_is_length_preserving` pins the invariant this rests
/// on.
fn uncommented(raw: &str, code: &str) -> String {
    raw.chars().take(code.chars().count()).collect()
}

/// End index (exclusive) of a format field at `at` that pads to a WIDTH —
/// `{:<28}`, `{name:>5.2}`, `{v:^label_w$}`, `{:width$}`, `{:9}` — or `None`
/// when `at` does not open one.
///
/// A width with no explicit alignment still lays out a column, and requiring
/// the alignment was this detector's first blind spot: `mcp_cmd.rs` writes
/// `"{:width$}  {:9}  {:7}  SOURCE"`, two padded columns and not one arrow
/// among them. Found by checking the scan against the A0 inventory's site
/// list rather than trusting the scan, which is the only reason the count
/// below is not two short.
///
/// What stays excluded: `{:.2}` carries no width at all, and `{:02}` /
/// `{:04}` carry one but are zero-padded NUMBERS — without that rule a
/// timestamp `"{:02}:{:02}"` reads as a hand-laid table row. The `0` flag is
/// the discriminator, and only when nothing else marks the field as a
/// column: `{:0>5}` is an explicitly aligned column and counts.
fn width_field_end(code: &str, at: usize) -> Option<usize> {
    let b = code.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let is_align = |c: u8| c == b'<' || c == b'>' || c == b'^';
    let mut i = at + 1;
    // optional argument name or index
    while i < b.len() && (is_ident(b[i]) || b[i] == b'.') {
        i += 1;
    }
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i += 1;
    // [[fill]align] — a fill char is only a fill if an align follows it
    let has_align = if i + 1 < b.len() && is_align(b[i + 1]) {
        i += 2;
        true
    } else if i < b.len() && is_align(b[i]) {
        i += 1;
        true
    } else {
        false
    };
    // sign / '#' / '0' flags sit between the align and the width
    let mut zero_padded = false;
    while i < b.len() && matches!(b[i], b'+' | b'-' | b'#' | b'0') {
        zero_padded |= b[i] == b'0';
        i += 1;
    }
    // Width is DIGITS or `name$` — never a bare identifier, which in this
    // position is the TYPE. `{:#x}` was the second blind spot: the `x` read
    // as a width, and four hex-dump lines in `cockpit/test_tty.rs` armed
    // themselves as hand-laid tables. Digits first, so `{:8x}` keeps its
    // width and hands the `x` to the tail scan.
    if i < b.len() && b[i].is_ascii_digit() {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        let start = i;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        if i == start || i >= b.len() || b[i] != b'$' {
            return None;
        }
        i += 1;
    }
    // A zero-padded number is not a column unless it also says which way it
    // leans — see this function's doc comment.
    if zero_padded && !has_align {
        return None;
    }
    // anything else in the spec, up to the closing brace
    while i < b.len() && b[i] != b'}' {
        if b[i] == b'"' || b[i] == b'{' {
            return None;
        }
        i += 1;
    }
    (i < b.len()).then(|| i + 1)
}

/// String literals carrying TWO OR MORE width fields.
///
/// One padded field is a label; **two in one literal is a table row laid out
/// by hand** — the shape the A0 inventory recorded, unarmed, as "the 22
/// ad-hoc two-width-field `format!` call sites" (SECTION 4.1.5). Grouping by
/// literal rather than by macro is deliberate: the same row shape appears in
/// `format!`, `println!`, `write!`, `push_str(&format!(..))` and `eprintln!`,
/// and a needle per macro would miss the sixth.
///
/// Boundaries are quotes, so two fields with no `"` between them are one
/// site. An escaped quote inside such a literal would split one site into
/// two — an OVER-count, which trips the ratchet rather than hiding a
/// duplicate, and no site in this workspace has one.
fn adhoc_column_sites(code: &str) -> usize {
    let b = code.as_bytes();
    let (mut sites, mut run, mut i) = (0usize, 0usize, 0usize);
    while i < b.len() {
        if b[i] == b'"' {
            if run >= 2 {
                sites += 1;
            }
            run = 0;
            i += 1;
        } else if b[i] == b'{' {
            match width_field_end(code, i) {
                Some(end) => {
                    run += 1;
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    if run >= 2 {
        sites += 1;
    }
    sites
}

const CATEGORIES: &[Category] = &[
    Category {
        name: "markdown parser sites",
        // Pinned to pulldown-cmark's constructor in a file that actually
        // imports it: `Parser::new` as a bare substring matched
        // `Parser::new_ext` only by luck and could not tell pulldown's
        // Parser from any other type named Parser (newt-cli already has a
        // private `struct Parser`) — so A1's own `+++` grammar work would
        // have tripped the very ratchet meant to guide it.
        count: |f| {
            if f.imports_pulldown {
                count_any(&f.squeezed, &["Parser::new_ext(", "Parser::new("])
            } else {
                0
            }
        },
        baseline: &[
            // C3a (#1857) ratcheted this from 2 sites to 1. The rows that
            // went are `agentic/markdown/mod.rs` and `newt-web/src/shell.rs`
            // — the two SURFACES, which no longer construct a parser at all;
            // they call `markup::dialect::parse`. What remains is that one
            // constructor. A row reappearing here is a surface choosing its
            // own dialect again, which is the divergence C3a deleted.
            ("newt-core/src/markup/dialect.rs", 1),
        ],
        rationale: "one Markdown parser constructor, on one option matrix. \
                    A1 (#1825) named both matrices in \
                    newt_core::markup::dialect; C3a (#1857) deleted the \
                    second (web Options::all) and made the surviving one \
                    unreachable except through dialect::parse. A second \
                    instantiation forks the dialect again",
    },
    Category {
        name: "question construction sites",
        count: |f| count_any(&f.squeezed, &["Question{", "Question::<"]),
        // **EMPTY — the category is closed (D0, #1878).** Not removed: an
        // empty baseline is the strongest form this row can take, because any
        // NEW site trips it. The needles and their anti-vacuous twin below
        // still run, so this is a live "never again" guard rather than a
        // deleted one.
        //
        // What went, and why each was the last of its kind:
        //
        // * `newt-core/src/agentic/tools.rs` — `mutation_confirm_question`
        //   built a legacy `Question` for the `--yolo` confirm; D0 builds the
        //   `InteractionDefinition` directly. The comment that stood here said
        //   "STAYS ... no slice of this epic deletes it", written before D0's
        //   scope named generic mutation confirmation. Corrected, not worked
        //   around.
        // * `newt-core/src/interaction_adapter.rs` — the `Question {` literal
        //   inside `definition_to_question`, whose last two production callers
        //   were `decode_answer` (now resolving through
        //   `newt_interaction::binding::resolve_typed`) and a renderability
        //   precondition in `await_web_decision` that its OWN comment said
        //   "retires with C3's removal of the reconstruction" — which C3c did.
        // * `newt-tui/src/permissions.rs` — `prompt_user_input`'s free-text
        //   form, already an `InteractionDefinition` since C0a; the row was
        //   stale rather than newly emptied, which only the scan revealed.
        //
        // Three consecutive slices predicted the adapter row would reach 0
        // "when the terminal decode moves onto the controller". It has, and so
        // did the other two.
        baseline: &[],
        rationale: "every place a user-facing Question is assembled outside \
                    the (future) one definition path; B0/D0 migrate these",
    },
    Category {
        name: "interaction answer validation sites",
        // A3 (#1837) closes a blind spot in the row above. "question
        // construction sites" counts by OLD-type syntax (`Question{`,
        // `Question::<`), so a validator written against the
        // newt-interaction types constructs no Question and adds ZERO
        // rows — the ratchet would report green while a third "is this a
        // legitimate answer" implementation accumulated beside
        // `Question::parse` and newt-web's `classify_decision`.
        //
        // Needles are type-shaped rather than name-shaped, because a
        // second implementation would be NEWLY NAMED: any production
        // function taking the protocol `&Response`, or returning A3's
        // accept/refuse verdict. They must begin at an identifier
        // boundary to be counted at all (see `count_sites`), which is why
        // they start at the `&` and the `<` rather than at `:` or `fn`.
        count: |f| {
            count_any(
                &f.squeezed,
                &["&Response)", "&Response,", "Accepted,Refusal>"],
            )
        },
        baseline: &[
            // The ONE sanctioned implementation: `validate_response`
            // (two `&Response` parameter sites plus its verdict type).
            // A2 froze the records; A3 validates them in exactly one
            // place, and B0 switches the surfaces onto it. A second file
            // appearing here is the sprawl this category exists to catch.
            ("newt-interaction/src/binding.rs", 3),
        ],
        rationale: "answer validation against the Newt Markup types belongs in \
                    ONE place (newt-interaction::binding); a second site is a \
                    third answer-validation implementation, which is the exact \
                    pattern the A0 inventory baselined for the old types",
    },
    Category {
        name: "console ask sites",
        // Receiver-explicit: a bare `.ask(` cannot tell `console.ask(` from
        // `gate.ask(` (the PermissionGate, seven sites in tools.rs) or a
        // mesh RPC's `asker.ask(`. Squeezing rejoins the rustfmt-split
        // calls that the old line-oriented rule needed a separate arm for.
        count: |f| {
            count_any(
                &f.squeezed,
                &["console.ask(", "console.ask_secret(", "window.ask("],
            )
        },
        baseline: &[
            // D1b-0 (#1892) split `setup.rs` into `setup/{mod,commit,tests}.rs`.
            // The row was REPOINTED, not lowered: all 23 sites moved intact to
            // `mod.rs` (the extracted transaction engine has no console at
            // all). The ratchet reported the old path as "Progress! Remove its
            // baseline row" — it cannot tell a rename from a deletion, and
            // taking that advice would have dropped a floor that never moved.
            //
            // D1b then took it 23 -> 19: the four `console.ask_secret` sites
            // are gone, funnelled into the ONE below.
            //
            // D1b-2 (#1903) took it 19 -> 13: four free-text prompts and two
            // `[Y/n]` decisions now build `InteractionDefinition`s and go out
            // through `Console::ask_definition`, which is armed separately
            // below rather than being allowed to vanish on a rename.
            ("newt-tui/src/setup/mod.rs", 13),
            // **A funnel, not a duplicate** — the justification the category
            // asks for. Four hidden prompts became one
            // `credentials::ask_secret`, which builds a `ControlKind::Secret`
            // definition and reads it through C1's seam; the console call that
            // remains is the TEST INJECTION POINT the scripted wizard drives,
            // and it goes out through `present_on_terminal` like every other
            // prompt. It reaches 0 when D1b-2/3 thread the seam and the
            // `Console` trait goes.
            ("newt-tui/src/setup/credentials.rs", 1),
            // D1a (#1885) ratcheted `newt-tui/src/crew_form.rs` off this
            // table entirely: its 7 `console.ask(` sites became one
            // `InteractionDefinition` state machine driven through C1's
            // `SurfaceInteraction` seam. `setup.rs` is D1b's, which is why
            // `line_console` itself is still here.
            ("newt-cli/src/dock_cmd.rs", 3),
            ("newt-cli/src/ocap_cmd.rs", 1),
            // The shared modal/prompt-window implementation itself — the
            // common path the other rows are duplicates BESIDE, pinned so the
            // shared layer does not quietly grow ask-shaped surface either.
            ("newt-core/src/tty/modal.rs", 5),
        ],
        rationale: "interactive ask/answer call sites outside the typed \
                    Question path; D0/D1 fold these into controller-backed \
                    forms",
    },
    Category {
        name: "definition-bridge asks",
        // `console.ask_definition(` does NOT match the needles above — the
        // `_` after `ask` stops `console.ask(` matching — so a site that
        // migrates to it drops out of "console ask sites" silently. That
        // would let a rename read as progress, which is gaming the gate, so
        // the bridge is armed here instead of disappearing.
        //
        // It is a genuinely different thing from the row above: a caller of
        // this has ALREADY modelled its prompt as an `InteractionDefinition`
        // and the terminal adapter derives `Echo` from the controls. It is
        // the typed path, reached through a trait that has not gone yet.
        count: |f| count_sites(&f.squeezed, "console.ask_definition("),
        baseline: &[("newt-tui/src/setup/mod.rs", 5)],
        rationale: "the D1b migration bridge: prompts already modelled as \
                    definitions, still delivered through the `Console` trait. \
                    It reaches 0 when D1b-3 retires the trait for C1's seam — \
                    these sites then pass a `SurfaceInteraction` directly",
    },
    Category {
        name: "prompt confirm helpers",
        count: |f| count_sites(&f.squeezed, "fn confirm_prompt("),
        baseline: &[
            ("newt-core/src/sas_confirm.rs", 1),
            ("newt-tui/src/rich_input.rs", 1),
        ],
        rationale: "bespoke confirm-prompt builders beside the Question path",
    },
    Category {
        name: "direct blocking reads",
        // Squeezed, so `io::stdin()\n    .read_line(..)` counts once and the
        // `.lock()` spelling is covered explicitly rather than by accident.
        count: |f| {
            count_any(
                &f.squeezed,
                &[
                    "stdin().read_line(",
                    "stdin().read(",
                    "stdin().lock().read_line(",
                    "stdin().lock().read(",
                ],
            )
        },
        baseline: &[
            ("newt-tui/src/line_console.rs", 2),
            ("newt-tui/src/lean_input.rs", 1),
            ("newt-tui/src/lib.rs", 1),
            ("newt-cli/src/mcp_probe_cmd.rs", 1),
            ("newt-cli/src/dgx_card.rs", 1),
            ("newt-cli/src/dgx.rs", 2),
            // THE ONE SANCTIONED READ, and the rationale below is corrected to
            // say so (#1909). This is `PromptWindow::read_line_into`'s own
            // body — the seal's implementation, not something the seal is
            // meant to replace. The old rationale said this category counted
            // reads "outside the sealed PromptWindow", which excluded the very
            // row it listed.
            //
            // Kept here rather than split into its own category: two
            // categories cannot share needles, because a file with hits
            // matches EVERY category whose count returns > 0 — verified, it
            // reports each file under both. So the definition is fixed instead
            // of the membership, and the ceiling is stated on the row.
            ("newt-core/src/tty/arbiter.rs", 1),
        ],
        rationale: "synchronous stdin reads; every one is a site to route \
                    through the semantic request seam EXCEPT the single \
                    sanctioned read inside the sealed PromptWindow, which is \
                    that seam's own implementation and stays at exactly one",
    },
    Category {
        name: "table renderer implementations",
        count: |f| {
            count_any(
                &f.squeezed,
                &[
                    "fn render_table(",
                    "fn tidy_markdown_tables(",
                    "fn render_panel(",
                    "push_html(",
                ],
            )
        },
        baseline: &[
            // THE one table algorithm (D3a, #1874) — the destination, listed
            // deliberately rather than named around the needle. A migrated
            // family's row disappears from this table; this row is what it
            // migrates ONTO, and it is the row that must stay at exactly 1.
            ("newt-core/src/markup/table.rs", 1),
            ("newt-core/src/agentic/markdown/table.rs", 1),
            // `newt-eval/src/scorecard.rs` was here at 1 until D3a: its
            // bespoke fixed-width renderer is deleted, and the type now
            // supplies rows to the algorithm above through `fmt::Display`.
            // `newt-eval/src/pyo3_module.rs` was here at 1 until D3b. It
            // never held an implementation — the needle was counting a
            // one-line BINDING (A0 §4.1.4, "pyo3 exposure"). Its Rust method
            // is now `table` under `#[pyo3(name = "render_table")]`, so
            // Python is unchanged and the file no longer DECLARES a table
            // renderer. A real one landing there trips as a NEW site file,
            // which is what makes dropping the row safe.
            // `newt-eval/tests/python_surface.rs` pins the Python name.
            // Both cfg(feature) arms of the same fn — production either way.
            // A0 §4.1.2: ZERO production callers, and the decision to delete
            // or wire it must consult wyvern-agent, so D3a leaves it.
            ("newt-core/src/agentic/mod.rs", 2),
            ("newt-tui/src/config_panel.rs", 1),
            ("newt-web/src/shell.rs", 1),
        ],
        rationale: "named table/document rendering implementations (the 22 \
                    ad-hoc two-width-field format! call sites are recorded \
                    unarmed in the A0 inventory; D3 owns them); one table \
                    algorithm is the D3 exit",
    },
    Category {
        name: "ad-hoc two-width-field format sites",
        count: |f| f.adhoc_sites,
        // Armed at 21 by SCAN, not by the inventory's "22" (D3b, #1886).
        // Every row below was read and confirmed to be a hand-laid column
        // row; the detector reports no site this list does not name.
        //
        // The three A0 §4.1.5 lines this shape deliberately EXCLUDES, each
        // carrying exactly ONE padded field, so they are not lost:
        //   newt-cli/src/ocap_cmd.rs         "  {:<5} {} ({}x)\n        {}\n"
        //   newt-eval/src/bin/newt-eval.rs   "  {:<28}  {}  {}"
        //   newt-cli/src/config_cmd.rs:52    "#   {name:<11}       {desc}"
        // plus the two `-----` rule lines A0 notes parenthetically
        // (tuning_cmd.rs, probe.rs). Widening to ONE field would sweep in
        // every `{:<5}` in the workspace — status lines, log prefixes — and
        // a tripwire that fires on everything reports nothing.
        baseline: &[
            ("newt-cli/src/config_cmd.rs", 1),
            ("newt-cli/src/dgx_card.rs", 3),
            ("newt-cli/src/dgx_status.rs", 1),
            ("newt-cli/src/dock_cmd.rs", 2),
            ("newt-cli/src/mcp_cmd.rs", 2),
            ("newt-cli/src/models_cmd.rs", 2),
            ("newt-cli/src/providers_cmd.rs", 2),
            ("newt-cli/src/tuning_cmd.rs", 2),
            ("newt-tui/src/chat.rs", 1),
            ("newt-tui/src/lib.rs", 2),
            ("newt-tui/src/probe.rs", 2),
            ("newt-tui/src/prompt.rs", 1),
        ],
        rationale: "hand-laid column rows: one string literal padding two or \
                    more fields to a width. The A0 inventory recorded these \
                    UNARMED; D3 owns them, and a category armed at its TRUE \
                    current count is what stops them being invisible",
    },
];

/// Append `code` to `out` with whitespace squeezed OUT of token joins but
/// PRESERVED between two identifier characters.
///
/// Deleting every space would join `match console` into `matchconsole`,
/// where the identifier-boundary check in [`count_sites`] then reads `h`
/// before `console` and rejects a real site — which is exactly how a
/// rustfmt-split `let raw = match console\n    .ask(..)` went missing while
/// the baseline was right. Dropping a run only when one of its neighbours is
/// punctuation joins the method chain without welding two words together.
fn squeeze_into(out: &mut String, code: &str) {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_whitespace() {
            out.push(c);
            continue;
        }
        while chars.peek().is_some_and(|n| n.is_whitespace()) {
            chars.next();
        }
        let before = out.chars().next_back();
        let after = chars.peek().copied();
        // A run between two identifier characters is a real token boundary.
        if before.is_some_and(is_ident) && after.is_some_and(is_ident) {
            out.push(' ');
        }
    }
    // A line ends where the next begins: keep the same rule across the seam
    // by leaving no trailing separator — the next call re-evaluates it.
}

/// Squeezed production code per file, for every production root.
fn production_code() -> BTreeMap<String, FileCode> {
    let root = workspace_root();
    let mut files: BTreeMap<String, FileCode> = BTreeMap::new();
    // `rel` allocates, and the visitor fires once per production LINE — so
    // remember the file it was last computed for.
    let mut last: Option<(std::path::PathBuf, String)> = None;
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, code, raw| {
            let name = match &last {
                Some((p, name)) if p == path => name.clone(),
                _ => {
                    let name = rel(&root, path);
                    last = Some((path.to_path_buf(), name.clone()));
                    name
                }
            };
            let entry = files.entry(name).or_insert_with(|| FileCode {
                squeezed: String::new(),
                imports_pulldown: false,
                adhoc_sites: 0,
            });
            if code.contains("pulldown_cmark") {
                entry.imports_pulldown = true;
            }
            squeeze_into(&mut entry.squeezed, code);
            entry.adhoc_sites += adhoc_column_sites(&uncommented(raw, code));
        },
    );
    files
}

fn production_counts() -> BTreeMap<(&'static str, String), usize> {
    let mut counts: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    for (path, code) in production_code() {
        for cat in CATEGORIES {
            let n = (cat.count)(&code);
            if n > 0 {
                counts.insert((cat.name, path.clone()), n);
            }
        }
    }
    counts
}

/// Workspace-relative path with forward slashes (Windows CI runs this too).
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn duplicate_site_baselines_only_ratchet_down() {
    let counts = production_counts();
    let mut problems = Vec::new();
    for cat in CATEGORIES {
        let expected: BTreeMap<&str, usize> = cat.baseline.iter().copied().collect();
        let actual: BTreeMap<String, usize> = counts
            .iter()
            .filter(|((name, _), _)| *name == cat.name)
            .map(|((_, path), n)| (path.clone(), *n))
            .collect();
        for (path, n) in &actual {
            match expected.get(path.as_str()) {
                None => problems.push(format!(
                    "[{}] NEW site file {path} ({n} hit(s)) — {}. Delete the \
                     duplicate, or justify a baseline addition in PR review.",
                    cat.name, cat.rationale
                )),
                Some(exp) if n > exp => problems.push(format!(
                    "[{}] {path} grew: {n} > baseline {exp} — {}.",
                    cat.name, cat.rationale
                )),
                Some(exp) if n < exp => problems.push(format!(
                    "[{}] {path} shrank: {n} < baseline {exp}. Progress! \
                     Ratchet down: update this file's baseline in the same PR.",
                    cat.name
                )),
                Some(_) => {}
            }
        }
        for (path, exp) in &expected {
            if !actual.contains_key(*path) {
                problems.push(format!(
                    "[{}] {path} no longer has any of its {exp} baselined \
                     site(s). Progress! Remove its baseline row in the same PR.",
                    cat.name
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "markup sprawl ratchet tripped:\n{}",
        problems.join("\n")
    );
}

/// `PromptSurface` (renderer identity inside permission policy — the exact
/// coupling B0 dissolves into frozen responder policy) stays confined to its
/// single current file, with its exact three branch sites. A new file
/// mentioning it, or a fourth branch, extends the coupling mid-migration.
/// **The renderer-identity discriminator, after B0a (#1841).**
///
/// This test used to be `prompt_surface_stays_confined_to_its_three_
/// branches`, matching the literal string `PromptSurface`. B0a deleted
/// that enum — and a ratchet keyed on the old name would have reported a
/// clean deletion while the SAME semantic branch point survived under a
/// new name in another crate. It did not disappear: it became
/// `newt_interaction::instance::Audience`, the same two variants, because
/// `validate_response` needs exactly this fact later (#1842).
///
/// So the test follows the discriminator rather than the name: audience
/// branching in permission POLICY stays confined to one file, with an
/// exact branch count.
#[test]
fn the_audience_discriminator_stays_confined_to_its_branches() {
    let files = production_code();
    let mentions: Vec<&String> = files
        .iter()
        .filter(|(path, code)| {
            path.starts_with("newt-tui/") && code.squeezed.contains("Audience::")
        })
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        mentions,
        vec!["newt-tui/src/permissions.rs"],
        "audience branching leaked beyond newt-tui/src/permissions.rs"
    );
    // The old enum must be gone: if it comes back, there are two
    // discriminators for one fact.
    assert!(
        files
            .values()
            .all(|c| !c.squeezed.contains("PromptSurface")),
        "PromptSurface came back alongside Audience"
    );

    let permissions = &files["newt-tui/src/permissions.rs"].squeezed;
    let branches = count_sites(permissions, "matches!(audience,")
        + count_sites(permissions, "match(tier,audience)");
    assert_eq!(
        branches, 2,
        "audience-branch count changed. B0a ratcheted the A0 baseline of 3 \
         DOWN to 2: the two identical `matches!(surface, Terminal)` tests \
         were always the same question and collapsed into one binding, \
         leaving one `matches!` + one `match (tier, audience)`. Down = \
         ratchet again; up = a new renderer-identity branch in policy"
    );
}

#[test]
fn action_model_registry_is_exact() {
    // (workspace-relative file, enum name, expected definition count there)
    const REGISTRY: &[(&str, &str, usize)] = &[
        (
            "newt-core/src/agentic/permissions.rs",
            "PermissionAction",
            1,
        ),
        (
            "newt-core/src/agentic/permissions.rs",
            "HumanQuestionOutcome",
            1,
        ),
        ("newt-core/src/store.rs", "Verdict", 1),
        // An unrelated eval Verdict shares the name; pinned so a rename or a
        // third Verdict cannot hide behind the store one.
        ("newt-core/src/symbols.rs", "Verdict", 1),
        ("newt-core/src/tty/modal.rs", "PromptLine", 1),
        ("newt-core/src/sas_confirm.rs", "SasVerdict", 1),
        ("newt-tui/src/vi.rs", "Confirm", 1),
        ("newt-tui/src/setup/mod.rs", "BackendChoice", 1),
        ("newt-tui/src/setup/mod.rs", "HostedProviderChoice", 1),
        ("newt-cli/src/dgx.rs", "ReconcileAction", 1),
    ];
    let root = workspace_root();
    let mut defs: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut names: Vec<&'static str> = REGISTRY.iter().map(|(_, n, _)| *n).collect();
    names.sort_unstable();
    names.dedup();
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, code, _| {
            for name in &names {
                let needle = format!("enum {name}");
                if let Some(at) = code.find(&needle) {
                    let after = code[at + needle.len()..].chars().next();
                    if matches!(after, None | Some(' ') | Some('<') | Some('{')) {
                        *defs.entry((rel(&root, path), *name)).or_default() += 1;
                    }
                }
            }
        },
    );
    let mut problems = Vec::new();
    for (file, name, expected) in REGISTRY {
        let n = defs.remove(&((*file).to_string(), *name)).unwrap_or(0);
        if n != *expected {
            problems.push(format!(
                "enum {name} in {file}: found {n} definition(s), baseline {expected} \
                 (deleted => remove its registry row; duplicated => a parallel \
                 action model mid-migration)"
            ));
        }
    }
    for ((file, name), n) in defs {
        problems.push(format!(
            "enum {name} gained a definition outside its registered file: \
             {file} ({n}) — a same-named parallel action model"
        ));
    }
    assert!(
        problems.is_empty(),
        "action-model registry drift:\n{}",
        problems.join("\n")
    );
}

/// **The legacy `Question` wire shape, frozen as an exact JSON golden.**
///
/// This replaces a count of `#[serde` attributes, which was not a wire
/// freeze at all: renaming `markdown` to `body`, changing `note` from
/// `Option<String>` to `String`, or swapping `skip_serializing_if` for a
/// `rename` all change persisted and web-facing JSON while leaving the
/// count at two — and a doc comment quoting `#[serde(` tripped it with
/// nothing on the wire changed. What the migration must not break is the
/// BYTES, so assert the bytes.
///
/// Both load-bearing omissions are covered: `aliases` disappears when
/// empty (so already-published web/DB JSON stays byte-identical) and
/// `note` disappears when `None`. Their round trip back through
/// `Question::parse` is what `store.rs` relies on to re-validate an
/// answered action inside its own transaction.
#[test]
fn the_question_wire_shape_is_frozen() {
    use newt_core::{Action, PermissionAction, Question};

    let full = Question {
        markdown: "\u{2298} run_command wants to run `bash`".to_string(),
        actions: vec![
            Action::new(PermissionAction::AllowOnce, "a", "allow once"),
            Action::new(PermissionAction::Deny, "d", "deny (default)").with_aliases(["n", "N"]),
        ],
        note: Some("Esc=back".to_string()),
    };
    assert_eq!(
        serde_json::to_string(&full).unwrap(),
        r#"{"markdown":"⊘ run_command wants to run `bash`","actions":[{"value":"allow_once","key":"a","label":"allow once"},{"value":"deny","key":"d","label":"deny (default)","aliases":["n","N"]}],"note":"Esc=back"}"#
    );

    // Empty aliases and a missing note are OMITTED, not rendered as
    // `[]`/`null` — the two `skip_serializing_if`s, pinned by their effect.
    let minimal: Question<PermissionAction> = Question {
        markdown: "m".to_string(),
        actions: vec![Action::new(PermissionAction::Deny, "d", "deny")],
        note: None,
    };
    assert_eq!(
        serde_json::to_string(&minimal).unwrap(),
        r#"{"markdown":"m","actions":[{"value":"deny","key":"d","label":"deny"}]}"#
    );

    // A payload written before `aliases` existed still deserializes, and
    // still authorizes exactly the displayed action.
    let legacy: Question<PermissionAction> = serde_json::from_str(
        r#"{"markdown":"m","actions":[{"value":"deny","key":"d","label":"deny"}]}"#,
    )
    .expect("pre-aliases payloads must keep deserializing");
    // D0 (#1878): the resolution moved to `newt_interaction::binding::resolve_typed`,
    // so the property is asserted through the path that now holds it. The WIRE
    // SHAPE above is what this test freezes and it has not moved; what changed
    // is which code answers "does this payload authorize the displayed action".
    let options = newt_core::interaction_adapter::question_to_definition(&legacy)
        .expect("a legacy payload still adapts")
        .controls
        .iter()
        .find_map(|c| match &c.kind {
            newt_interaction::ControlKind::Choice { options } => Some(options.clone()),
            _ => None,
        })
        .expect("the decision control is a choice");
    assert_eq!(
        newt_interaction::binding::resolve_typed(&options, "d")
            .and_then(|o| newt_core::interaction_adapter::action_for_option(o.as_str())),
        Some(PermissionAction::Deny)
    );
    assert_eq!(
        newt_interaction::binding::resolve_typed(&options, "a"),
        None,
        "an undisplayed action never resolves"
    );

    // Every wire name, so a rename cannot hide behind an unchanged count.
    for (action, wire) in [
        (PermissionAction::AllowOnce, "allow_once"),
        (PermissionAction::AllowSession, "allow_session"),
        (PermissionAction::AllowPermanent, "allow_permanent"),
        (PermissionAction::Deny, "deny"),
        (PermissionAction::DenyAlways, "deny_always"),
        (PermissionAction::DenyPermanent, "deny_permanent"),
        (PermissionAction::Back, "back"),
        (PermissionAction::Exit, "exit"),
    ] {
        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            format!("\"{wire}\""),
            "PermissionAction wire name changed"
        );
        assert_eq!(action.as_str(), wire, "as_str must match the wire name");
    }
}

/// The scanner scans real code — a walker that visits nothing reports
/// success forever.
/// Every baselined path must actually fall under a production root. A
/// scoping mistake (a member dropped from the manifest, a rename, a crate
/// moved out of the workspace) would otherwise turn into a SILENT gap: the
/// file stops being scanned, its count reads 0, and the down-only ratchet
/// invites someone to delete the row as "progress".
#[test]
fn every_baselined_path_is_inside_a_production_root() {
    let root = workspace_root();
    let roots = production_roots(&root);
    assert!(!roots.is_empty(), "the workspace manifest yielded no roots");
    let mut missing = Vec::new();
    let baselined = CATEGORIES
        .iter()
        .flat_map(|c| c.baseline.iter().map(|(p, _)| *p))
        .chain(["newt-tui/src/permissions.rs"]);
    for rel_path in baselined {
        let abs = root.join(rel_path);
        if !roots.iter().any(|r| abs.starts_with(r)) {
            missing.push(rel_path.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "baselined paths outside every production root (they would silently \
         read as zero): {missing:?}"
    );
}

#[test]
fn the_ratchet_scans_the_real_workspace() {
    let root = workspace_root();
    let mut files = std::collections::BTreeSet::new();
    let mut saw_anchor = false;
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, code, _| {
            files.insert(path.to_path_buf());
            // B0a (#1841) renamed the anchor: `permission_question_for`
            // split into a policy half and a marshalling half, and the
            // marshalling half is the one that builds the definition.
            if code.contains("fn permission_definition") {
                saw_anchor = true;
            }
        },
    );
    assert!(
        files.len() > 100,
        "scanner visited only {} files — the walk is broken",
        files.len()
    );
    assert!(
        saw_anchor,
        "scanner never saw permission_definition in newt-tui — production \
         lines are not being visited"
    );
}

/// **A needle inside a trailing comment is not a site.** Truncating
/// trailing line comments is a semantic tightening of the shared scanner
/// (it also governs first_principle's caller law), and it points the
/// dangerous way for an EXACT-count ratchet: because a comment is not a
/// hit, a comment can no longer SUBSTITUTE for a deleted real site and
/// hold a baseline steady while the site disappears. Pinned rather than
/// left to the commit message.
#[test]
fn a_needle_in_a_trailing_comment_is_not_a_site() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "fn f() { g(); } // like Parser::new_ext(a, b) but not\n\
         // Parser::new_ext(c, d) in a whole-line comment either\n\
         fn real() { let p = Parser::new_ext(e, f); }\n",
    )
    .unwrap();

    let mut hits = 0;
    for_each_production_line(
        &[root.path().to_path_buf()],
        &no_extra_skips,
        &mut |_, code, _| {
            hits += code.matches("Parser::new_ext(").count();
        },
    );
    assert_eq!(hits, 1, "only the real call is a site");
}

/// **A quote inside a char literal must not open a string.**
/// `strip_string_literals` blanks string contents so a needle in an error
/// message cannot satisfy a law. It treated the `"` in the char literal
/// `'"'` as a string opener, blanking the REST of the line — including the
/// brace that opens a block. Inside a `#[cfg(test)]` region (the real
/// instance is `newt-tui/src/palette.rs:582`, `for banned in ['|', '<',
/// '[', '"'] {`) the tracker then loses an opening brace, the depth
/// unwinds early, and the remaining test-only lines are visited as
/// production — feeding EXACT counts with test scaffolding.
///
/// Regression (#1823 review A0-6): against the old stripper this failed
/// with `test-only lines scanned as production: ["letp=Parser::new_ext(a,b);"]`.
#[test]
fn a_quote_in_a_char_literal_does_not_open_a_string() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        // The blanked `{` unbalances the tracker, so the depth reaches
        // zero one closing brace EARLY and everything after it — still
        // inside `mod tests` — reads as production.
        "#[cfg(test)]\nmod tests {\n    fn t() {\n        for banned in ['|', '\"'] {\n            noop();\n        }\n    }\n    fn later() {\n        let p = Parser::new_ext(a, b);\n    }\n}\n",
    )
    .unwrap();

    let mut visited = Vec::new();
    for_each_production_line(
        &[root.path().to_path_buf()],
        &no_extra_skips,
        &mut |_, code, _| {
            if code.contains("Parser::new") {
                let mut squeezed = String::new();
                squeeze_into(&mut squeezed, code);
                visited.push(squeezed);
            }
        },
    );
    assert!(
        visited.is_empty(),
        "test-only lines scanned as production: {visited:?}"
    );
}

/// **Needles survive a line split and name their receiver.** The old
/// needles were single-line and receiver-blind, which fails in both
/// directions: rustfmt splitting a call DROPPED a site (the count falls,
/// the ratchet says "Progress! Ratchet down", and a baseline that should
/// not move gets lowered — after which the next real duplicate lands
/// green), while a bare `.ask(` counted any receiver at all, so a
/// `gate.ask(` or a mesh `asker.ask(` reformatted onto its own line would
/// report as a NEW interactive prompt.
///
/// Regression (#1823 review A0-4/A0-5). Counting now runs over
/// whitespace-normalized code with explicit receivers.
/// **Anti-vacuous twin for "interaction answer validation sites".** A
/// category whose needles match nothing would sit at baseline forever and
/// report green while the sprawl it names accumulated — which is exactly
/// the blind spot this category was added to close, so it must not
/// reproduce it. This proves the counter fires on a second validator
/// written against the new types, survives a rustfmt line split, and does
/// NOT fire on the near-misses that share a prefix.
#[test]
fn a_second_answer_validator_against_the_new_types_is_counted() {
    let count = |src: &str| {
        let mut squeezed = String::new();
        for line in src.lines() {
            squeeze_into(&mut squeezed, line);
        }
        let file = FileCode {
            squeezed,
            imports_pulldown: false,
            adhoc_sites: adhoc_column_sites(src),
        };
        (CATEGORIES
            .iter()
            .find(|c| c.name == "interaction answer validation sites")
            .unwrap()
            .count)(&file)
    };

    // A newly-named second validator — the shape the old-type needles are
    // blind to, since it constructs no `Question`.
    assert_eq!(
        count("fn is_a_legitimate_answer(r: &Response) -> bool { true }"),
        1,
        "a second validator taking the protocol Response was not counted"
    );
    // Split across lines by rustfmt: still one site.
    assert_eq!(
        count("fn check(\n    definition: &InteractionDefinition,\n    response: &Response,\n) -> bool { true }"),
        1
    );
    // The verdict type is counted wherever a second implementation
    // returns it.
    assert_eq!(
        count("fn second_opinion(r: &Response) -> Result<Accepted, Refusal> { todo!() }"),
        2,
        "the parameter and the verdict shape are both sites"
    );

    // Near-misses that must NOT count. `validate_responses_request` is
    // the unrelated OpenAI Responses wire gate in newt-core; the others
    // merely share a prefix with `Response`.
    for near_miss in [
        "fn validate_responses_request(body: &Body) -> Result<Validated, Error> { todo!() }",
        "fn attribute(p: &ResponderProvenance, t: &ResponseTag) -> bool { true }",
        "fn identify(id: &ResponseId,) -> bool { true }",
    ] {
        assert_eq!(count(near_miss), 0, "false positive on: {near_miss}");
    }

    // And the category is armed against the real workspace: if the
    // baseline ever drops to nothing, the count is not measuring code.
    let counts = production_counts();
    let live: usize = counts
        .iter()
        .filter(|((name, _), _)| *name == "interaction answer validation sites")
        .map(|(_, n)| *n)
        .sum();
    assert!(
        live > 0,
        "the category matches nothing in the real workspace, so it is decoration"
    );
}

#[test]
fn needles_survive_a_line_split_and_name_their_receiver() {
    let code = |src: &str| {
        let mut squeezed = String::new();
        for line in src.lines() {
            squeeze_into(&mut squeezed, line);
        }
        FileCode {
            squeezed,
            imports_pulldown: src.contains("pulldown_cmark"),
            adhoc_sites: adhoc_column_sites(src),
        }
    };
    let count = |name: &str, src: &str| {
        let file = code(src);
        (CATEGORIES.iter().find(|c| c.name == name).unwrap().count)(&file)
    };

    // Split by rustfmt — still one site, in both spellings the repo has.
    assert_eq!(
        count(
            "console ask sites",
            "let k = console\n    .ask_secret(\"key\")?;"
        ),
        1
    );
    assert_eq!(
        count(
            "console ask sites",
            "let r = match console\n    .ask(\"q\")?;"
        ),
        1,
        "a keyword before the receiver must not weld into it"
    );
    assert_eq!(
        count(
            "direct blocking reads",
            "let n = io::stdin()\n    .read_line(&mut buf)?;"
        ),
        1
    );

    // Receiver-explicit: neither of these is an interactive console prompt.
    assert_eq!(count("console ask sites", "gate.ask(&req)?;"), 0);
    assert_eq!(
        count("console ask sites", "let r = asker\n    .ask(m)?;"),
        0
    );
    // ...and a longer identifier ending in the receiver name is its own thing.
    assert_eq!(count("console ask sites", "my_console.ask(\"q\")?;"), 0);

    // The parser needle is pinned to pulldown-cmark, so an unrelated type
    // named Parser (newt-cli already has one) is not a dialect fork.
    assert_eq!(
        count("markdown parser sites", "let p = Parser::new(input);"),
        0,
        "a Parser in a file that never names pulldown_cmark is not the dialect"
    );
    assert_eq!(
        count(
            "markdown parser sites",
            "use pulldown_cmark::Parser;\nlet p = Parser::new_ext(src, opts);"
        ),
        1
    );
}

/// **Parent-gated test children are excluded structurally, not by name.**
/// A `#[cfg(test)] mod x;` child carries no cfg of its own, so a line
/// scanner reading it alone calls test scaffolding production code. Two
/// name allowlists (`*_test.rs` here, `lib_tests/` in the shared scanner)
/// covered only the children someone had noticed; the repo has seven more
/// under `mod_tests/`, `tools_tests/`, and `newt-skills/src/tests.rs`. A
/// test-only `Question { .. }`, `console.ask(`, or `stdin().read_line` in
/// any of them reports as a NEW production site, and the natural reaction
/// is to add a third allowlist name.
///
/// Regression (#1823 review A0-3): against the name allowlists this failed
/// listing the children they missed, led by
/// `newt-core/src/agentic/mod_tests/anthropic_loop.rs`.
#[test]
fn parent_gated_test_children_are_excluded_structurally() {
    let root = workspace_root();
    let mut visited = std::collections::BTreeSet::new();
    for_each_production_line(&production_roots(&root), &|_| false, &mut |path, _, _| {
        visited.insert(rel(&root, path));
    });

    // Declared out of line behind `#[cfg(test)]`, by `#[path = "..."]` or by
    // a plain `mod x;` — every form the repo actually uses.
    let children = [
        "newt-core/src/agentic/mod_tests/anthropic_loop.rs",
        "newt-core/src/agentic/mod_tests/http_loop.rs",
        "newt-core/src/agentic/mod_tests/artifact_provenance.rs",
        "newt-core/src/agentic/mod_tests/bat_largest_files.rs",
        "newt-core/src/agentic/tools_tests/private_url_mcp_bat.rs",
        "newt-skills/src/tests.rs",
        "newt-tui/src/lib_tests/core.rs",
        "newt-tui/src/prompt_visibility_test.rs",
        "newt-core/src/tty/pty_notice_test.rs",
    ];
    let leaked: Vec<_> = children.iter().filter(|c| visited.contains(**c)).collect();
    assert!(
        leaked.is_empty(),
        "parent-gated test children scanned as production: {leaked:?}"
    );
    // The declaring parents ARE production — an exclusion that swallowed
    // them would make the whole scan vacuous.
    for parent in [
        "newt-core/src/agentic/mod.rs",
        "newt-skills/src/lib.rs",
        "newt-tui/src/lib.rs",
    ] {
        assert!(
            visited.contains(parent),
            "the declaring parent must still be scanned: {parent}"
        );
    }
}

/// **A trailing comment on a gated brace-less item must not blind the rest
/// of the file.** `#[cfg(test)] mod x;` is skipped by a pending-attribute
/// latch that clears at the item's semicolon. Truncating trailing line
/// comments (added so a needle inside a comment is not a hit) leaves the
/// whitespace that preceded the `//`, so `mod tests; // out of line` ends
/// with a SPACE, the latch never clears, and every later line in the file
/// goes invisible — the exact blindness the brace tracker was built to fix.
///
/// Consequence if it regresses: production sites silently vanish from the
/// counts, the ratchet reports "Progress! Ratchet down", and someone
/// lowers a baseline that should not move — after which the next real
/// duplicate lands green.
///
/// Regression (#1823 review A0-2): against the untrimmed check this failed
/// with `production lines after a gated item are invisible: []`.
#[test]
fn a_trailing_comment_on_a_gated_item_does_not_blind_the_file() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "#[cfg(test)]\nmod tests; // declared out of line\n\nfn real() { Parser::new_ext(s, o); }\n",
    )
    .unwrap();

    let mut visited = Vec::new();
    for_each_production_line(
        &[root.path().to_path_buf()],
        &no_extra_skips,
        &mut |_, code, _| {
            if code.contains("Parser::new") {
                visited.push(code.to_string());
            }
        },
    );
    assert_eq!(
        visited.len(),
        1,
        "production lines after a gated item are invisible: {visited:?}"
    );
}

/// **The manifest is parsed as TOML, not scanned for a needle.**
///
/// Root discovery is a gate: if it under-reports members, the scan
/// narrows silently and every absence-style law ("no production caller
/// does X") passes because nothing was scanned. A hand-rolled reader
/// scanning for `members = [` and splitting the array by LINES fails on
/// two shapes cargo accepts:
///
/// - the inline form `members = ["a", "b"]`, where the whole array is one
///   line that does not begin with a quote; and
/// - a `default-members` key BEFORE `members`, whose name contains the
///   needle, so the wrong array is read.
///
/// Regression (#1824 external review A): against the hand-rolled reader
/// both fixtures produced ZERO members. Zero is not a loud failure —
/// `production_roots` still returns `newt-web/src`, so the root list is
/// non-empty and first_principle's laws would quietly scan one crate.
#[test]
fn the_inline_members_form_yields_every_member() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"alpha\", \"beta\"]\n",
    )
    .unwrap();
    for m in ["alpha", "beta"] {
        std::fs::create_dir_all(root.path().join(m).join("src")).unwrap();
    }
    assert_eq!(
        production_roots(root.path()),
        vec![
            root.path().join("alpha").join("src"),
            root.path().join("beta").join("src"),
        ]
    );
}

#[test]
fn default_members_does_not_shadow_the_real_members_list() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\ndefault-members = [\"alpha\"]\nmembers = [\n    \"alpha\",\n    \"beta\",\n]\n",
    )
    .unwrap();
    for m in ["alpha", "beta"] {
        std::fs::create_dir_all(root.path().join(m).join("src")).unwrap();
    }
    assert_eq!(
        production_roots(root.path()).len(),
        2,
        "`default-members` contains the substring `members` — reading it \
         instead of the real list silently narrows the scan"
    );
}

/// A globbed member must still expand: a `crates/*` entry that resolves to
/// nothing is the same invisible gap in a different disguise.
#[test]
fn a_globbed_member_expands_against_the_filesystem() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    )
    .unwrap();
    for m in ["crates/one", "crates/two"] {
        std::fs::create_dir_all(root.path().join(m).join("src")).unwrap();
    }
    assert_eq!(production_roots(root.path()).len(), 2);
}

/// The real workspace still yields exactly today's root set — the rewrite
/// must not move the scope it establishes.
#[test]
fn the_real_workspace_root_set_is_unchanged() {
    let roots = production_roots(&workspace_root());
    assert_eq!(
        roots.len(),
        22,
        "21 production members + newt-web; a change here rescopes every \
         law and every baseline. Moved 21 -> 22 by #1828 A2.0, which adds \
         the `newt-interaction` protocol crate — the ratchet noticing a new \
         member is this assertion working, not breaking"
    );
    let names: Vec<String> = roots
        .iter()
        .map(|r| rel(&workspace_root(), r))
        .filter(|r| r.starts_with("tests/"))
        .collect();
    assert!(
        names.is_empty(),
        "test-support members are not production: {names:?}"
    );
}

/// **The walk is scoped to production workspace members.** Walking
/// "everything under the repo root" swept in crates the workspace
/// deliberately `exclude`s (newt-mesh, whose `.ask(` is a mesh RPC, not an
/// interactive prompt), the `tests/common` and `tests/pty` support crates
/// (whose `src/` passes any "is it under src?" filter), and anything else a
/// checkout happens to nest. Every one of those is a NEW-site false positive
/// that fails on a developer's machine and cannot be reproduced from CI.
///
/// Regression (#1823 review A0-1/A0-8): against the old walker — which took
/// one root and filtered by "any path component is `src`" — this failed with
/// four visited files instead of one, including `tests/common/src/lib.rs`
/// and the excluded crate.
#[test]
fn the_walk_is_scoped_to_production_workspace_members() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\n    \"alpha\",\n    \"tests/common\",\n]\nexclude = [\"excluded\"]\n",
    )
    .unwrap();
    let needle = "let parser = Parser::new_ext(src, opts);\n";
    for rel in [
        "alpha/src",        // a production member — the only one in scope
        "tests/common/src", // a test-support MEMBER: not production
        "excluded/src",     // workspace-excluded crate
        "stray/src",        // not a member at all
    ] {
        let dir = root.path().join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), needle).unwrap();
    }

    let scan = |roots: &[std::path::PathBuf]| {
        let mut seen = Vec::new();
        for_each_production_line(roots, &no_extra_skips, &mut |path, code, _| {
            if code.contains("Parser::new") {
                seen.push(rel(root.path(), path));
            }
        });
        seen.sort();
        seen
    };

    // The shape this test exists to prevent, made visible: rooting the walk
    // at the repo directory sweeps in all four trees.
    assert_eq!(
        scan(&[root.path().to_path_buf()]).len(),
        4,
        "an unscoped walk sees every nested tree — the pre-fix behavior"
    );
    // Scoped by manifest: exactly the production member.
    assert_eq!(
        scan(&production_roots(root.path())),
        vec!["alpha/src/lib.rs".to_string()],
        "only production workspace members are in scope"
    );
}

/// **The walker must never descend into a nested worktree.** This repo's
/// own convention puts scratch/crew worktrees at `<repo>/.worktrees/`
/// (`.gitignore:107`), and the main checkout really does carry full `src`
/// trees there. `first_principle`'s laws tolerate the copies (they need at
/// least one caller and a copy is still one), but THIS file's exact
/// per-file counts and its "NEW site file" detection do not: a second copy
/// of `newt-tui/src/permissions.rs` is a new path with its own hits, so the
/// ratchet would trip for anyone running the suite from the main checkout
/// while CI stayed green on a clean one — a red pre-push hook nobody can
/// reproduce from the CI logs.
///
/// Regression (#1823 review): the skip list was `target|.git|.claude|docs`
/// and missed `.worktrees`. Against that list this test failed with
/// `the walker descended into a nested worktree: [".worktrees/x/newt-core/src/lib.rs"]`.
/// The fix skips every HIDDEN directory, which subsumes `.git`, `.claude`,
/// `.worktrees`, and `.github` — a rule that cannot be out-enumerated by
/// the next dot-directory convention someone adds.
#[test]
fn the_walker_never_descends_into_hidden_directories() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("newt-core/src");
    let hidden = root.path().join(".worktrees/x/newt-core/src");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&hidden).unwrap();
    // The same needle in both trees: one real, one a nested-worktree copy.
    let needle = "let parser = Parser::new_ext(src, opts);\n";
    std::fs::write(real.join("lib.rs"), needle).unwrap();
    std::fs::write(hidden.join("lib.rs"), needle).unwrap();

    let mut visited = Vec::new();
    // Root the walk AT the tempdir so the hidden-dir rule is what is under
    // test here, not the workspace scoping (covered by its own test).
    for_each_production_line(
        &[root.path().to_path_buf()],
        &no_extra_skips,
        &mut |path, code, _| {
            if code.contains("Parser::new") {
                visited.push(rel(root.path(), path));
            }
        },
    );

    let strays: Vec<_> = visited
        .iter()
        .filter(|p| p.starts_with('.') || p.contains("/."))
        .collect();
    assert!(
        strays.is_empty(),
        "the walker descended into a nested worktree: {strays:?}"
    );
    assert_eq!(
        visited,
        vec!["newt-core/src/lib.rs".to_string()],
        "exactly the real tree is scanned"
    );
}

/// **C3a (#1857): one dialect, one option matrix, one parser constructor.**
///
/// A1 (#1825) named both matrices so neither call site chose undocumented
/// defaults; it deliberately did not unify them. C3a's deletion gate is
/// *"remove the second independent Markdown option matrix"*, and these two
/// guards are what make a third one unable to come back quietly.
///
/// They are structural on purpose. "Which extensions are enabled" is a fact
/// about the program text, not about any value the program computes: a
/// behavioral test can prove that *today's* two callers agree, but only a
/// source scan can prove a *third* caller cannot appear next week choosing
/// its own matrix. That is the failure mode the A0 inventory recorded — two
/// parser sites, two matrices, neither documented — and it is the one this
/// module exists to prevent recurring.
mod c3a {
    /// The one module allowed to name pulldown's `Options` or build a
    /// `Parser`. Everything else asks it.
    const DIALECT: &str = "newt-core/src/markup/dialect.rs";

    /// A function returning a pulldown `Options` — i.e. an option matrix.
    ///
    /// Squeezed code welds `)` to `->` to `Options`, so this is the shape a
    /// matrix definition takes regardless of how rustfmt broke the line.
    const MATRIX_RETURN: &str = "->Options";

    /// Option-matrix definitions in one file's squeezed production code.
    fn matrix_defs(squeezed: &str) -> usize {
        super::count_sites(squeezed, MATRIX_RETURN)
    }

    /// Pulldown `Parser` constructions in one file's squeezed production
    /// code. Same needles the "markdown parser sites" category uses.
    fn parser_sites(squeezed: &str) -> usize {
        super::count_any(squeezed, &["Parser::new_ext(", "Parser::new("])
    }

    /// **Both surfaces reach the parser through one constructor.**
    ///
    /// Not "both pass the same options" — *neither gets to pass options at
    /// all*. The dialect is unrepresentable at a call site, so a second
    /// matrix is not a thing a reviewer has to catch.
    #[test]
    fn both_surfaces_parse_with_one_option_matrix() {
        let code = super::production_code();
        let mut problems = Vec::new();

        // Anti-vacuous half FIRST as a fact, not an early return: an empty
        // or mis-rooted scan must fail loudly rather than report "no strays".
        let sanctioned = code
            .get(DIALECT)
            .map(|f| parser_sites(&f.squeezed))
            .unwrap_or(0);
        if sanctioned != 1 {
            problems.push(format!(
                "{DIALECT} must hold exactly ONE parser construction (the \
                 sanctioned one); the scan saw {sanctioned}. A zero here \
                 usually means the scan is not seeing the file at all, which \
                 would make the stray check below vacuous."
            ));
        }

        for (path, file) in &code {
            if path == DIALECT || !file.imports_pulldown {
                continue;
            }
            let n = parser_sites(&file.squeezed);
            if n > 0 {
                problems.push(format!(
                    "{path} constructs a pulldown Parser {n}× — it must call \
                     newt_core::markup::dialect::parse instead, so it cannot \
                     choose its own option matrix"
                ));
            }
        }
        assert!(problems.is_empty(), "C3a: {}", problems.join("\n"));
    }

    /// **A second option matrix cannot return.**
    ///
    /// Exactly one function in production returns a pulldown `Options`, and
    /// it lives in the dialect module. `web_enhancement_options` was the
    /// second; deleting it is C3a's deletion gate, and this is what keeps
    /// the gate shut.
    #[test]
    fn a_second_option_matrix_cannot_return() {
        let code = super::production_code();
        let mut found: Vec<(String, usize)> = code
            .iter()
            .filter(|(_, f)| f.imports_pulldown)
            .map(|(path, f)| (path.clone(), matrix_defs(&f.squeezed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        found.sort();

        assert_eq!(
            found,
            vec![(DIALECT.to_string(), 1)],
            "exactly one option matrix must exist, in {DIALECT}. Found: \
             {found:?}. A second matrix is the divergence A1 froze and C3a \
             deleted — widen the one dialect instead, in its own slice."
        );
    }

    /// **Anti-vacuous twin.** Both guards above are "count and compare", so
    /// a detector that silently matched nothing would report a clean repo.
    /// These feed it source it MUST see, and source it must NOT.
    #[test]
    fn the_c3a_detectors_can_fail() {
        // A matrix definition is seen, however rustfmt broke the line…
        assert_eq!(matrix_defs("pub fn canonical_options()->Options{}"), 1);
        assert_eq!(
            matrix_defs("pub fn a()->Options{}pub fn b()->Options{}"),
            2,
            "a second matrix must be visible to the guard"
        );
        // …and a function returning something else is not a matrix.
        assert_eq!(matrix_defs("pub fn name()->String{}"), 0);
        assert_eq!(matrix_defs(""), 0);

        // Parser constructions likewise.
        assert_eq!(parser_sites("Parser::new_ext(src,opts)"), 1);
        assert_eq!(parser_sites("Parser::new(src)"), 1);
        assert_eq!(
            parser_sites("MyParser::new(src)"),
            0,
            "a longer identifier ending in the needle is its own thing"
        );
        assert_eq!(parser_sites("dialect::parse(src)"), 0);
    }
}

/// **C3c (#1867): the web surface reconstructs no legacy `Question`.**
///
/// C3's deletion gate is *"remove permission-card-specific model
/// reconstruction and the second independent Markdown option matrix"*. C3a
/// took the option matrix; this is the other half, armed so it cannot come
/// back.
///
/// Structural rather than behavioural on purpose: a behavioural test proves
/// today's card renders without a `Question`, but only a source scan proves a
/// future card cannot quietly reintroduce one — which is exactly how the
/// reconstruction arrived in the first place (B0b-2 added
/// `PendingOffer::question` as a convenience for a renderer).
mod c3c {
    /// The crate that must be free of the legacy type.
    const WEB: &str = "newt-web/src/";

    /// Reconstructions of a legacy `Question` in one file's squeezed
    /// production code.
    ///
    /// The needles are the two ways the web can reach one, and both are
    /// METHOD/function calls rather than the type name. That distinction cost
    /// this guard a vacuous green on its first run: the reconstruction is
    /// invoked as `p.question()` and bound with an inferred type, so the word
    /// `Question` never appears in the web crate at all and a type-name needle
    /// matched nothing while the reconstruction sat right there.
    ///
    /// * `question()` — `PendingOffer::question`, the round trip C3c deletes.
    ///   Counted with the shared identifier-boundary rule, so the `.` before
    ///   it is a boundary and `my_question()` is not a hit.
    /// * `definition_to_question(` — the adapter underneath it. Banning only
    ///   the wrapper would leave the web free to call the adapter directly and
    ///   rebuild exactly what was removed.
    fn question_refs(squeezed: &str) -> usize {
        super::count_any(squeezed, &["question()", "definition_to_question("])
    }

    #[test]
    fn the_web_surface_reconstructs_no_legacy_question() {
        let code = super::production_code();
        let mut offenders: Vec<String> = code
            .iter()
            .filter(|(path, _)| path.starts_with(WEB))
            .filter_map(|(path, f)| {
                let n = question_refs(&f.squeezed);
                (n > 0).then(|| format!("{path} names the legacy Question type {n}×"))
            })
            .collect();
        offenders.sort();

        // Anti-vacuous: the scan must actually be seeing the web crate. A
        // mis-rooted scan would report "no offenders" about nothing at all.
        let seen = code.keys().filter(|p| p.starts_with(WEB)).count();
        assert!(
            seen >= 4,
            "the scan saw only {seen} file(s) under {WEB} — it is not reading \
             the web crate, which would make the check below vacuous"
        );

        assert!(
            offenders.is_empty(),
            "C3c: the web renders and decodes from the InteractionDefinition \
             directly; reconstructing a Question is the model reconstruction \
             C3's deletion gate removes.\n{}",
            offenders.join("\n")
        );
    }

    /// **Anti-vacuous twin.** The guard is a `!contains`, which a detector
    /// that matched nothing would satisfy perfectly.
    #[test]
    fn the_c3c_detector_can_see_a_reconstruction() {
        // Both routes to a reconstruction are visible…
        assert_eq!(question_refs("let q = p.question()?;"), 1);
        assert_eq!(question_refs("definition_to_question(&d)"), 1);
        assert_eq!(
            question_refs("let a = p.question()?;let b = definition_to_question(&d);"),
            2
        );
        // …and the TYPE NAME alone is not, which is the blind spot that gave
        // this guard a vacuous green before the needles were fixed.
        assert_eq!(
            question_refs("let q: Question<PermissionAction> = x;"),
            0,
            "the type name is not how the web reaches a reconstruction"
        );
        // A longer identifier ending in the needle is its own thing.
        assert_eq!(question_refs("self.my_question()"), 0);
        assert_eq!(question_refs(""), 0);
    }
}

/// **The anti-vacuous twin for the ad-hoc column category (D3b, #1886).**
///
/// The category counts a SHAPE inside string literals, and every other
/// category in this file counts identifiers in code the scanner has already
/// blanked literals out of. A shape detector has failure modes a needle does
/// not, and this one had two — both found by checking the scan against the A0
/// inventory's site list instead of believing the scan.
mod d3b {
    use super::{adhoc_column_sites, uncommented};

    #[test]
    fn the_adhoc_detector_can_see_a_new_column_row() {
        // Two padded fields in one literal is a hand-laid row. All three
        // spellings in the armed baseline are recognised.
        assert_eq!(
            adhoc_column_sites(r#"println!("{:<20} {:<18} scope", a, b)"#),
            1
        );
        assert_eq!(
            adhoc_column_sites(r#"w!("{:width$}  {:9}  {:7}  SOURCE")"#),
            1
        );
        assert_eq!(
            adhoc_column_sites(r#"format!("{name:<11} {slash:<3}  {d}")"#),
            1
        );
        // Two literals are two sites, so a file cannot hide a row by
        // adding it next to another.
        assert_eq!(adhoc_column_sites(r#"f("{:<3}{:<3}"); g("{:<3}{:<3}")"#), 2);

        // ONE padded field is a label, not a row — the line that keeps this
        // from firing on every status print in the workspace.
        assert_eq!(
            adhoc_column_sites(r#"println!("  {:<28}  {}  {}", a, b, c)"#),
            0
        );
        // A precision is not a width.
        assert_eq!(adhoc_column_sites(r#"format!("{:.2} {:.2}", a, b)"#), 0);
        // BLIND SPOT 1 — zero-padded numbers. A clock is not a table.
        assert_eq!(
            adhoc_column_sites(r#"format!("{:02}:{:02}:{:02}", h, m, s)"#),
            0
        );
        // BLIND SPOT 2 — `{:#x}`'s `x` is the TYPE, not a width. Four
        // hex-dump lines in cockpit/test_tty.rs armed themselves as tables
        // before this rule existed.
        assert_eq!(
            adhoc_column_sites(r#"format!("c_lflag {:#x} -> {:#x}", a, b)"#),
            0
        );
        // …but an explicitly aligned zero-pad IS a column, and a width that
        // carries a type keeps its width.
        assert_eq!(adhoc_column_sites(r#"format!("{:0>5} {:0>5}", a, b)"#), 1);
        assert_eq!(adhoc_column_sites(r#"format!("{:8x} {:8x}", a, b)"#), 1);

        assert_eq!(adhoc_column_sites(""), 0);
    }

    /// A row shape inside a trailing comment is prose, not a site — and the
    /// cut that removes it is measured in CHARS, because blanking a literal
    /// turns a multi-byte char into a one-byte `_`. With byte indices this
    /// slices the line in the wrong place and the row is lost.
    #[test]
    fn a_multi_byte_literal_does_not_shift_the_comment_cut() {
        let raw = "row(\"… {:<3} {:<3}\"); // note {:<9} {:<9}";
        let code = {
            let mut c = crate::common::strip_string_literals(raw);
            if let Some(i) = c.find("//") {
                c.truncate(i);
            }
            c
        };
        let cut = uncommented(raw, &code);
        assert!(cut.contains("{:<3} {:<3}"), "the row survives: {cut:?}");
        assert!(!cut.contains("{:<9}"), "the comment does not: {cut:?}");
        assert_eq!(adhoc_column_sites(&cut), 1);
    }

    /// [`uncommented`] rests on this: `strip_string_literals` replaces each
    /// char with exactly one char. If that ever stops holding, the comment
    /// cut silently lands in the wrong place and rows go missing — a SHRINK,
    /// which reads as "Progress! Ratchet down" and lowers a floor that
    /// should not move.
    #[test]
    fn strip_string_literals_is_length_preserving() {
        for line in [
            "let a = 1;",
            "row(\"… {:<3}\")",
            "let c = 'x'; let d = '\\n';",
            "let s = \"a\\\"b\";",
            "// {:<3} {:<3}",
            "",
        ] {
            assert_eq!(
                crate::common::strip_string_literals(line).chars().count(),
                line.chars().count(),
                "char count must be preserved: {line:?}"
            );
        }
    }
}

/// **D1a (#1885): the crew form asks through the semantic seam, and only it.**
///
/// The flow used to be parameterised over `line_console::Console` — a private
/// ask/say I/O path that could only ever reach a cooked terminal. It is now
/// `InteractionDefinition` state driven through `SurfaceInteraction`, so the
/// same form renders on the plain projection and the rich TUI without a
/// form-specific implementation of either.
///
/// Keyed on the whole `crew_form` module rather than one file: the slice
/// split it into `mod.rs` + `state.rs`, and a gate that named the old path
/// would have reported success while the console came back next door.
#[test]
fn the_crew_form_carries_no_private_console_path() {
    let files = production_code();
    let crew: Vec<&String> = files
        .keys()
        .filter(|p| p.starts_with("newt-tui/src/crew_form"))
        .collect();
    assert!(
        crew.len() >= 2,
        "expected the crew_form module's files, found {crew:?}"
    );
    for path in &crew {
        assert!(
            !files[*path].squeezed.contains("line_console"),
            "{path} reaches for the line console again — the crew form asks \
             through SurfaceInteraction (C1) and nothing else"
        );
    }

    // ANTI-VACUOUS TWIN. Two ways this guard could pass while proving
    // nothing, both closed here:
    //
    // 1. The needle finds nothing anywhere (a typo, a renamed module). It
    //    must still fire where the console genuinely IS used.
    // 2. `line_console` was DELETED rather than retired from this flow.
    //    D1a's scope is the crew form; `setup.rs` still needs the module and
    //    is D1b's to migrate, so removing it here would be out of scope AND
    //    would make the assertion above trivially true.
    assert!(
        files.contains_key("newt-tui/src/line_console.rs"),
        "the line console module itself must survive D1a — setup.rs (D1b) \
         still uses it"
    );
    // Keyed on the setup MODULE, not one path. D1b-0 (#1892) split
    // `setup.rs` into `setup/{mod,commit,tests}.rs` and this assertion —
    // written against the old path — failed by panicking on a missing index
    // key. It did its job (it noticed), but it should notice a CONSOLE
    // coming back, not a file being renamed. Both halves of the twin now
    // name modules, matching the crew_form side above.
    assert!(
        files
            .iter()
            .any(|(path, code)| path.starts_with("newt-tui/src/setup")
                && code.squeezed.contains("line_console")),
        "the needle no longer matches the setup module, which DOES use the \
         line console — the guard above is passing vacuously"
    );
}
