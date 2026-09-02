//! **Terminal takers are registered, and the registration is two-way (#2027).**
//!
//! `Terminal::suspend_for_prompt` is defined in the same file as `RegionLease`
//! and, until #2027, never consulted the region table. #2019 is what that
//! costs: `/settings` took its own prompt window while the cockpit had an
//! editor mounted below, producing two live chevrons, a modal with no rows
//! reserved, and a header repainting through the question every 250 ms. The
//! call had been reviewed as correct wiring.
//!
//! A ratchet on the NUMBER of acquisitions would have passed that PR
//! unchanged — the call site already existed; what was wrong was the context
//! it ran in. So the guard is a **registration**, and it is modelled on the
//! one two-way conformance check this repository already runs:
//! `rich_input`'s `every_ladder_claimant_is_reachable_from_the_editors_own_state`,
//! which fails when a rung in `assets/esc_ladder.toml` has no live accessor
//! **and** when an accessor has no rung.
//!
//! The two directions here:
//!
//! * **A declared taker with no acquisition site fails.** A variant sitting in
//!   `TerminalTaker::ALL` that nothing names is a dead row — exactly the shape
//!   the ladder's absent rung-1 note warns about, where a conformance test
//!   starts passing on a constant.
//! * **An acquisition site with no declaration fails.** Half of this is the
//!   compiler's: the argument is required, and
//!   `tests/ui/suspend_for_prompt_requires_a_taker.rs` pins that there is no
//!   argument-free door. The half the compiler cannot see is an acquisition
//!   that passes a taker *computed elsewhere* — `suspend_for_prompt(t)` — which
//!   compiles fine and moves the declaration away from the state that
//!   justifies it. That is what this scan refuses.
//!
//! A source scan is the right shape for the same reason the ladder's is: the
//! property is a fact about the program text (is every taker reachable, is
//! every acquisition declared), not about a value the program computes. The
//! scanner is the shared one in `tests/common`, which skips `#[cfg(test)]`
//! regions by brace depth and parent-gated out-of-line test children
//! structurally — so a test-only acquisition is not counted as a surface.

use std::collections::BTreeMap;
use std::path::Path;

use newt_core::tty::TerminalTaker;

mod common;
use common::{for_each_production_line, production_roots, workspace_root};

/// The shared scanner already excludes build output, hidden directories, and
/// parent-gated test children structurally; this check adds nothing.
fn no_extra_skips(_: &Path) -> bool {
    false
}

/// Both public doors, matched by the one prefix they share. The private
/// builder (`Self::suspend_for_prompt_with_output`) is deliberately NOT
/// matched: it is the inside of the seal, not an acquisition.
const DOOR: &str = "Terminal::suspend_for_prompt";

/// One production acquisition: where it is, and what it declared.
#[derive(Debug)]
struct Acquisition {
    file: String,
    /// The variant named inline at the call, or `None` when the call passed
    /// something this scan cannot see as a declaration.
    declared: Option<String>,
}

/// Every production `Terminal::suspend_for_prompt{,_to}` call in the
/// workspace, with the taker each one names.
///
/// Runs over each file's **whitespace-squeezed** production code, because
/// rustfmt splits most of these calls across three lines and a line-at-a-time
/// needle would find the door and lose the argument.
fn acquisitions() -> Vec<Acquisition> {
    let root = workspace_root();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<(std::path::PathBuf, String)> = None;
    for_each_production_line(
        &production_roots(&root),
        &no_extra_skips,
        &mut |path, code, _raw| {
            let name = match &last {
                Some((p, name)) if p == path => name.clone(),
                _ => {
                    let name = path
                        .strip_prefix(&root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    last = Some((path.to_path_buf(), name.clone()));
                    name
                }
            };
            let squeezed = files.entry(name).or_default();
            squeezed.extend(code.chars().filter(|c| !c.is_whitespace()));
        },
    );

    let mut found = Vec::new();
    for (file, squeezed) in &files {
        let mut from = 0;
        while let Some(at) = squeezed[from..].find(DOOR) {
            let call = from + at;
            from = call + DOOR.len();
            let Some(args) = argument_list(&squeezed[from..]) else {
                continue;
            };
            from += args.len();
            found.push(Acquisition {
                file: file.clone(),
                declared: declared_taker(&args),
            });
        }
    }
    found
}

/// The text between the call's parentheses, or `None` when what follows the
/// name is not a call at all (a `[`Terminal::suspend_for_prompt`]` reference
/// in a plain `//` comment is already gone, but a path used as a value is
/// not).
fn argument_list(after_name: &str) -> Option<String> {
    let rest = after_name
        .strip_prefix("_to(")
        .or(after_name.strip_prefix("("))?;
    let mut depth = 1_usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The single `TerminalTaker::Variant` named in an argument list, or `None`
/// when the call named none — or named more than one, which is a call this
/// scan must not silently pick a winner from.
fn declared_taker(args: &str) -> Option<String> {
    const NEEDLE: &str = "TerminalTaker::";
    let mut names = Vec::new();
    let mut from = 0;
    while let Some(at) = args[from..].find(NEEDLE) {
        let start = from + at + NEEDLE.len();
        from = start;
        let end = args[start..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(args.len(), |n| start + n);
        if end > start {
            names.push(args[start..end].to_string());
        }
    }
    (names.len() == 1).then(|| names.remove(0))
}

/// **Both directions, in the shape the Esc ladder's reachability test uses.**
#[test]
fn every_terminal_taker_is_declared_at_an_acquisition_and_every_acquisition_declares_one() {
    let found = acquisitions();

    // POSITIVE READ ASSERTION. Both halves below are absence-shaped, and an
    // absence check fails OPEN: anything that shrinks the scanned text makes
    // it likelier to pass. If the scan reads nothing, say so here rather than
    // reporting a clean bill of health.
    assert!(
        found.len() >= 15,
        "the production scan found only {} acquisitions — it read almost \
         nothing, and every assertion below would be vacuous",
        found.len()
    );

    // Direction 1: an acquisition site with no declaration. The compiler
    // catches a MISSING argument; this catches a declaration hoisted away
    // from the state that justifies it.
    let undeclared: Vec<&str> = found
        .iter()
        .filter(|a| a.declared.is_none())
        .map(|a| a.file.as_str())
        .collect();
    assert!(
        undeclared.is_empty(),
        "these acquisitions name no single `TerminalTaker::` variant at the \
         call: {undeclared:?} — the declaration belongs AT the acquisition, \
         beside the state that justifies it, the way the Esc ladder's claim \
         accessors sit beside `Vi`'s own fields"
    );

    // Direction 2: a declared taker with no acquisition site. A dead row here
    // is a rung nothing can ever fire.
    let mut declared: Vec<&str> = found.iter().filter_map(|a| a.declared.as_deref()).collect();
    declared.sort_unstable();
    declared.dedup();
    let mut table: Vec<&str> = TerminalTaker::ALL.iter().map(|t| t.name()).collect();
    table.sort_unstable();
    assert_eq!(
        declared, table,
        "the taker table and the surfaces that actually acquire the terminal \
         have drifted apart — a name on the left with no row is an undeclared \
         surface, a row on the right with no name is a taker that can never fire"
    );
}

/// The cockpit modal is the ONE production acquisition through the `File`
/// door, and it is the taker that legitimately takes rows it did not reserve
/// from the arbiter's point of view.
///
/// Pinned separately because the two-way check above is set-shaped: it would
/// still pass if `CockpitModal` were declared from some other file. Where a
/// deliberate row-taker lives is exactly the fact review needs to be able to
/// find, and `exactly_two_takers_take_rows_they_do_not_own` in the arbiter is
/// the count half of the same ratchet.
#[test]
fn the_two_deliberate_row_takers_are_where_they_say_they_are() {
    let found = acquisitions();
    let site_of = |name: &str| -> Vec<String> {
        found
            .iter()
            .filter(|a| a.declared.as_deref() == Some(name))
            .map(|a| a.file.clone())
            .collect()
    };
    assert_eq!(
        site_of("CockpitModal"),
        vec!["newt-tui/src/cockpit/presenter.rs".to_string()],
        "the cockpit modal is the presenter's, and only the presenter's"
    );
    assert_eq!(
        site_of("PermissionAuthorization"),
        vec!["newt-tui/src/permissions.rs".to_string()],
        "the authorization prompt is the permission gate's, and only its"
    );
}
