//! In-process regex line search for the embedded `grep` tool.
//!
//! Built on ripgrep's own library crate `grep-regex` for the actual regex
//! matching (so the agent has a real `grep` on machines where every
//! `run_command` spawn can be refused — notably macOS). This needs only
//! `fs_read`, exactly like `read_file`: it walks the workspace with `ignore`
//! (the same walker the `find` tool uses) and matches each file in-process —
//! no shell, no subprocess.
//!
//! The execution arm lives in `tools.rs` and reuses `find`'s permission checks
//! verbatim; this module is the search itself.

use globset::{Glob, GlobMatcher};
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use std::io::BufRead;

/// Options parsed from the tool's JSON args (the arm supplies the defaults for
/// the ones that are optional). Kept separate from the arm so it stays
/// unit-testable.
pub(crate) struct GrepOpts<'a> {
    pub pattern: &'a str,
    pub glob: Option<&'a str>,
    pub ignore_case: bool,
    pub context: usize,
    pub max_results: usize,
}

/// The default result cap: the same floor `find` advertises.
pub(crate) const DEFAULT_MAX_RESULTS: usize = 100;

/// Walk `root` the same way `find` does (gitignore-aware, prunes
/// target/node_modules, no symlink following), searching every matching file
/// in-process. `on_line` is called once per output line in discovery order.
pub(crate) fn grep_search(
    root: &std::path::Path,
    workspace_root: &std::path::Path,
    opts: &GrepOpts<'_>,
    mut on_line: impl FnMut(&str),
) -> Result<(), String> {
    // The user pattern is a regex; fail loudly on a bad one rather than
    // searching nothing. Built once and reused per file.
    let matcher = build_matcher(opts.pattern, opts.ignore_case)?;

    // Optional glob filter against the workspace-relative path (e.g. "*.rs").
    let glob: Option<GlobMatcher> = match opts.glob {
        Some(g) if !g.is_empty() => Some(
            Glob::new(g)
                .map_err(|e| format!("invalid glob: {e}"))?
                .compile_matcher(),
        ),
        _ => None,
    };

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    if ob.add("!target/").is_ok() && ob.add("!node_modules/").is_ok() {
        if let Ok(ov) = ob.build() {
            builder.overrides(ov);
        }
    }

    for result in builder.build() {
        let entry = match result {
            // Skip individual unreadable entries rather than failing the walk.
            Ok(e) => e,
            Err(_) => continue,
        };
        // depth 0 is the search root itself; directories are never searched.
        if entry.depth() == 0 || entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(workspace_root)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        // A `glob` filter matches against the workspace-relative path (with
        // `/` normalised), mirroring `find`'s name-glob behaviour.
        if let Some(g) = &glob {
            if !g.is_match(&rel) {
                continue;
            }
        }
        // A read error on one file is not fatal: skip it and keep going.
        let _ = search_file(entry.path(), &rel, &matcher, opts.context, &mut on_line);
    }
    Ok(())
}

fn build_matcher(pattern: &str, ignore_case: bool) -> Result<RegexMatcher, String> {
    if pattern.is_empty() {
        return Err("empty pattern".to_string());
    }
    let mut b = grep_regex::RegexMatcherBuilder::new();
    b.case_insensitive(ignore_case);
    b.build(pattern).map_err(|e| format!("invalid pattern: {e}"))
}

/// Read a file line by line and match each line with the compiled matcher.
/// Binary files (any NUL byte) are skipped silently, like `grep`. Context
/// lines are drawn before/after each hit and use the `-` line slot, as grep.
fn search_file(
    path: &std::path::Path,
    rel: &str,
    matcher: &RegexMatcher,
    context: usize,
    on_line: &mut impl FnMut(&str),
) -> Result<(), String> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        // Unreadable → skip, mirroring find.
        Err(_) => return Ok(()),
    };
    let reader = std::io::BufReader::new(file);
    // A NUL byte in the file means binary: skip silently (grep's -I).
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // A non-UTF8 byte is treated as binary: stop reading this file.
            Err(_) => return Ok(()),
        };
        if line.as_bytes().contains(&b'\0') {
            return Ok(());
        }
        lines.push(line);
    }
    // An empty file yields no matches; the caller validates `pattern` up front.
    if lines.is_empty() {
        return Ok(());
    }
    // `matcher` (built once, above) encodes the validated pattern; `NoError`
    // from its `is_match` is `()`, so map the error to a message then `?` it.
    let total = lines.len();
    for (idx, line) in lines.iter().enumerate() {
        // `Matcher::is_match` takes bytes and returns `Result<bool, NoError>`;
        // `NoError` is `()`, so map the error to a message then `?` it.
        if !matcher.is_match(line.as_bytes()).map_err(|_| "invalid pattern".to_string())? {
            // Only the hit lines are printed; context is drawn from the lines
            // around each hit below. Skip non-hits outright.
            continue;
        }
        // Emit context lines preceding the hit.
        for n in idx.saturating_sub(context)..idx {
            on_line(&format!("{rel}:-:{}", lines[n]));
        }
        on_line(&format!("{rel}:{}:{}", idx + 1, line));
        // Emit context lines following the hit.
        for n in (idx + 1)..(idx + 1 + context).min(total) {
            on_line(&format!("{rel}:-:{}", lines[n]));
        }
    }
    Ok(())
}
