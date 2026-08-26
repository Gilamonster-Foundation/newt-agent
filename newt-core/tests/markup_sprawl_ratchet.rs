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
            ("newt-core/src/agentic/markdown/mod.rs", 1),
            ("newt-web/src/shell.rs", 1),
        ],
        rationale: "two Markdown parsers with two option matrices (TUI dialect \
                    vs web Options::all) is the dialect fork the epic's A1/C3 \
                    slices own; a third instantiation forks the dialect again",
    },
    Category {
        name: "question construction sites",
        count: |f| count_any(&f.squeezed, &["Question{", "Question::<"]),
        baseline: &[
            ("newt-tui/src/permissions.rs", 2),
            ("newt-core/src/agentic/tools.rs", 1),
        ],
        rationale: "every place a user-facing Question is assembled outside \
                    the (future) one definition path; B0/D0 migrate these",
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
            ("newt-tui/src/setup.rs", 23),
            ("newt-tui/src/crew_form.rs", 7),
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
            // The ONE sanctioned blocking read: the sealed PromptWindow path
            // under the line arbiter. It stays at exactly one.
            ("newt-core/src/tty/arbiter.rs", 1),
        ],
        rationale: "synchronous stdin reads outside the sealed PromptWindow; \
                    C0/C1 route these through the semantic request seam",
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
            ("newt-core/src/agentic/markdown/table.rs", 1),
            ("newt-eval/src/scorecard.rs", 1),
            ("newt-eval/src/pyo3_module.rs", 1),
            // Both cfg(feature) arms of the same fn — production either way.
            ("newt-core/src/agentic/mod.rs", 2),
            ("newt-tui/src/config_panel.rs", 1),
            ("newt-web/src/shell.rs", 1),
        ],
        rationale: "named table/document rendering implementations (the 22 \
                    ad-hoc two-width-field format! call sites are recorded \
                    unarmed in the A0 inventory; D3 owns them); one table \
                    algorithm is the D3 exit",
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
        &mut |path, code, _| {
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
            });
            if code.contains("pulldown_cmark") {
                entry.imports_pulldown = true;
            }
            squeeze_into(&mut entry.squeezed, code);
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
#[test]
fn prompt_surface_stays_confined_to_its_three_branches() {
    let files = production_code();
    let mentions: Vec<&String> = files
        .iter()
        .filter(|(_, code)| code.squeezed.contains("PromptSurface"))
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        mentions,
        vec!["newt-tui/src/permissions.rs"],
        "PromptSurface leaked beyond newt-tui/src/permissions.rs"
    );
    let permissions = &files["newt-tui/src/permissions.rs"].squeezed;
    let branches = count_sites(permissions, "matches!(surface,")
        + count_sites(permissions, "match(tier,surface)");
    assert_eq!(
        branches, 3,
        "surface-branch count changed from the A0 baseline of 3 (two \
         matches!(surface, ..) + one match (tier, surface)); down = ratchet \
         the baseline, up = a new renderer-identity branch in policy"
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
        ("newt-tui/src/setup.rs", "BackendChoice", 1),
        ("newt-tui/src/setup.rs", "HostedProviderChoice", 1),
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
    assert_eq!(legacy.parse("d"), Some(PermissionAction::Deny));
    assert_eq!(
        legacy.parse("a"),
        None,
        "an undisplayed action never parses"
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
            if code.contains("fn permission_question_for") {
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
        "scanner never saw permission_question_for in newt-tui — production \
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
        21,
        "20 production members + newt-web; a change here rescopes every \
         law and every baseline"
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
