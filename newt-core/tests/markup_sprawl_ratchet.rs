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
use common::{for_each_production_line, workspace_root};

/// In-src test modules reached through a parent-side `#[cfg(test)] mod x;`
/// declaration (repo convention: `*_test.rs`). The child file carries no cfg
/// of its own, so the line scanner cannot see the gate; skip by name. This
/// exclusion is ratchet-local — `first_principle.rs` deliberately scans them.
fn parent_gated_test_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with("_test.rs"))
}

/// One armed category: a needle matched against production code lines
/// (string-literal contents blanked), and the exact per-file baseline.
struct Category {
    name: &'static str,
    /// (code-line, trimmed-code-line) -> does this line hold one site?
    matches: fn(&str, &str) -> bool,
    /// Workspace-relative path -> exact expected production line count.
    baseline: &'static [(&'static str, usize)],
    rationale: &'static str,
}

/// `needle` preceded by a non-identifier char (so `Question {` cannot match
/// `PendingQuestion {`).
fn word_hit(code: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = code[from..].find(needle) {
        let at = from + i;
        let boundary = at == 0
            || !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary {
            return true;
        }
        from = at + needle.len();
    }
    false
}

const CATEGORIES: &[Category] = &[
    Category {
        name: "markdown parser sites",
        matches: |code, _| code.contains("Parser::new"),
        baseline: &[
            ("newt-core/src/agentic/markdown/mod.rs", 1),
            ("newt-web/src/shell.rs", 1),
        ],
        rationale: "two Markdown parsers with two option matrices (TUI dialect \
                    vs web Options::all) is the dialect fork A1 unifies; a \
                    third instantiation forks the dialect again",
    },
    Category {
        name: "question construction sites",
        matches: |code, _| word_hit(code, "Question {") || word_hit(code, "Question::<"),
        baseline: &[
            ("newt-tui/src/permissions.rs", 2),
            ("newt-core/src/agentic/tools.rs", 1),
        ],
        rationale: "every place a user-facing Question is assembled outside \
                    the (future) one definition path; B0/D0 migrate these",
    },
    Category {
        name: "console ask sites",
        matches: |code, ctrim| {
            code.contains("console.ask(")
                || code.contains("console.ask_secret(")
                || code.contains("window.ask(")
                || ctrim.starts_with(".ask(")
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
        matches: |code, _| code.contains("fn confirm_prompt("),
        baseline: &[
            ("newt-core/src/sas_confirm.rs", 1),
            ("newt-tui/src/rich_input.rs", 1),
        ],
        rationale: "bespoke confirm-prompt builders beside the Question path",
    },
    Category {
        name: "direct blocking reads",
        matches: |code, _| {
            code.contains("stdin()") && (code.contains(".read_line(") || code.contains(".read("))
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
        matches: |code, _| {
            code.contains("fn render_table(")
                || code.contains("fn tidy_markdown_tables(")
                || code.contains("fn render_panel(")
                || code.contains("push_html(")
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

fn production_counts() -> BTreeMap<(&'static str, String), usize> {
    let root = workspace_root();
    let mut counts: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    for_each_production_line(&root, &parent_gated_test_file, &mut |path, code, _raw| {
        let ctrim = code.trim_start();
        for cat in CATEGORIES {
            if (cat.matches)(code, ctrim) {
                *counts.entry((cat.name, rel(&root, path))).or_default() += 1;
            }
        }
    });
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
    let root = workspace_root();
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    let mut branches = 0usize;
    for_each_production_line(&root, &parent_gated_test_file, &mut |path, code, _| {
        if code.contains("PromptSurface") {
            *files.entry(rel(&root, path)).or_default() += 1;
        }
        if rel(&root, path) == "newt-tui/src/permissions.rs"
            && (code.contains("matches!(surface,") || code.contains("match (tier, surface)"))
        {
            branches += 1;
        }
    });
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        vec!["newt-tui/src/permissions.rs"],
        "PromptSurface leaked beyond newt-tui/src/permissions.rs: {files:?}"
    );
    assert_eq!(
        branches, 3,
        "surface-branch count changed from the A0 baseline of 3 (two \
         matches!(surface, ..) + one match (tier, surface)); down = ratchet \
         the baseline, up = a new renderer-identity branch in policy"
    );
}

/// The enumerated action-model registry: every enum that models
/// user-selectable actions/outcomes, by (file, name). Down-only; a NEWLY
/// NAMED parallel action enum cannot be detected by name and is caught in
/// review with the A0 inventory as the checklist.
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
    for_each_production_line(&root, &parent_gated_test_file, &mut |path, code, _| {
        for name in &names {
            let needle = format!("enum {name}");
            if let Some(at) = code.find(&needle) {
                let after = code[at + needle.len()..].chars().next();
                if matches!(after, None | Some(' ') | Some('<') | Some('{')) {
                    *defs.entry((rel(&root, path), *name)).or_default() += 1;
                }
            }
        }
    });
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

/// Wire-shape tripwires for the legacy `Question` contract A0 freezes: the
/// serde surface of `Question`/`Action` (exactly two attributes, both
/// load-bearing per their doc comments) and the eight `PermissionAction` wire
/// renames. Any drift here changes persisted/web JSON mid-migration.
#[test]
fn frozen_wire_shapes_are_unchanged() {
    let root = workspace_root();
    let question =
        std::fs::read_to_string(root.join("newt-core/src/tty/widgets/question.rs")).unwrap();
    let serde_attrs = question.matches("#[serde").count();
    assert_eq!(
        serde_attrs, 2,
        "Question/Action serde attribute count changed from the frozen 2 \
         (aliases + note) — the legacy wire shape moved during migration"
    );

    let permissions =
        std::fs::read_to_string(root.join("newt-core/src/agentic/permissions.rs")).unwrap();
    let start = permissions
        .find("permission_actions! {")
        .expect("the permission_actions! invocation exists");
    let block = brace_block(&permissions[start..]);
    assert_eq!(
        block.matches("=> \"").count(),
        8,
        "PermissionAction wire-rename arm count changed from the frozen 8"
    );
    for wire in [
        "\"allow_once\"",
        "\"allow_session\"",
        "\"allow_permanent\"",
        "\"deny\"",
        "\"deny_always\"",
        "\"deny_permanent\"",
        "\"back\"",
        "\"exit\"",
    ] {
        assert!(
            block.contains(wire),
            "PermissionAction wire string {wire} left the permission_actions! \
             block — a wire rename mid-migration"
        );
    }
}

/// The scanner scans real code — a walker that visits nothing reports
/// success forever.
#[test]
fn the_ratchet_scans_the_real_workspace() {
    let root = workspace_root();
    let mut files = std::collections::BTreeSet::new();
    let mut saw_anchor = false;
    for_each_production_line(&root, &parent_gated_test_file, &mut |path, code, _| {
        files.insert(path.to_path_buf());
        if code.contains("fn permission_question_for") {
            saw_anchor = true;
        }
    });
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
    for_each_production_line(
        root.path(),
        &parent_gated_test_file,
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

/// From the start of `text` (which begins at the `permission_actions!`
/// invocation), the substring up to the matching close of its first `{`.
fn brace_block(text: &str) -> &str {
    let open = text.find('{').expect("invocation has a block");
    let mut depth = 0i32;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[..open + i + 1];
                }
            }
            _ => {}
        }
    }
    text
}
