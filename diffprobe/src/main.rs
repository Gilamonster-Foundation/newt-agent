//! diffprobe — a concept probe (NOT part of the newt workspace).
//!
//! Question it answers: *can an LLM inside a harness use watchdiff-tui to show
//! the diff it just made and explore that diff with the human?*
//!
//! Two surfaces, matching newt's amphibious design:
//!   1. Headless / structured — emits a JSON report the model reads and
//!      re-renders as GFM markdown into chat (the plain-scroller path).
//!      MEASURED: works, see FINDINGS.md.
//!   2. Interactive — watchdiff-tui's own ratatui review TUI.
//!      MEASURED: NOT reachable for an agent-authored diff. `TuiApp::new`
//!      takes a `FileWatcher` and renders *its* event log
//!      (`src/core/watcher.rs` keeps `previous_contents` and diffs on each
//!      fs event). There is no accept/reject/apply-to-disk anywhere in the
//!      crate — the only writes are `export` (patch files). Believed
//!      otherwise until the source was read; see FINDINGS.md.
//!
//! This probe wires the *headless* diff-generation path and prints a real diff
//! against the working tree, so the concept is measured, not believed.

use anyhow::{Context, Result};
use serde::Serialize;
use std::process::Command;

use watchdiff_tui::diff::{DiffAlgorithmType, DiffFormatter, DiffGenerator};

#[derive(Serialize)]
struct Op {
    kind: &'static str,
    line: String,
}

#[derive(Serialize)]
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    ops: Vec<Op>,
}

#[derive(Serialize)]
struct FileDiff {
    path: String,
    kind: &'static str,
    added: usize,
    removed: usize,
    algorithm: String,
    hunks: Vec<Hunk>,
    unified: String,
}

#[derive(Serialize)]
struct Report {
    base: String,
    head: String,
    files: Vec<FileDiff>,
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("spawning git")?;
    if !out.status.success() {
        anyhow::bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Read the contents of `path` at ref `r`, or None if it does not exist there.
fn show_at(r: &str, p: &str) -> Option<String> {
    let out = Command::new("git")
        .args(&["show", &format!("{r}:{p}")])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

fn file_diff(gen: &DiffGenerator, path: &str, base: &str) -> Result<FileDiff> {
    let old = show_at(base, path);
    let new = show_at("HEAD", path);
    let (old, new, kind) = match (&old, &new) {
        (Some(o), Some(n)) => (o.as_str(), n.as_str(), "modified"),
        (None, Some(n)) => ("", n.as_str(), "added"),
        (Some(_), None) => ("", "", "deleted"),
        (None, None) => anyhow::bail!("no content for {path}"),
    };
    let result = gen.generate(old, new);
    let unified = DiffFormatter::format_unified(&result, path, path);
    let hunks: Vec<Hunk> = result
        .hunks
        .iter()
        .map(|h| Hunk {
            old_start: h.old_start,
            old_len: h.old_len,
            new_start: h.new_start,
            new_len: h.new_len,
            ops: h
                .operations
                .iter()
                .map(|op| {
                    let (kind, line): (&'static str, String) = match op {
                        watchdiff_tui::diff::DiffOperation::Equal(s) => ("equal", s.clone()),
                        watchdiff_tui::diff::DiffOperation::Insert(s) => ("insert", s.clone()),
                        watchdiff_tui::diff::DiffOperation::Delete(s) => ("delete", s.clone()),
                    };
                    Op { kind, line }
                })
                .collect(),
        })
        .collect();
    Ok(FileDiff {
        path: path.to_string(),
        kind,
        added: result.stats.lines_added,
        removed: result.stats.lines_removed,
        algorithm: gen.algorithm_name().to_string(),
        hunks,
        unified,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let base = args.get(0).map(String::as_str).unwrap_or("HEAD~1");
    let format = args.get(1).map(String::as_str).unwrap_or("json");

    let head = git(&["rev-parse", "--short", "HEAD"])?;
    let names = git(&["diff", "--name-only", &format!("{base}...HEAD")])?;
    let paths: Vec<String> = names
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let gen = DiffGenerator::new(DiffAlgorithmType::Myers);
    let mut files = Vec::new();
    for p in &paths {
        if let Ok(fd) = file_diff(&gen, p, base) {
            files.push(fd);
        }
    }

    let report = Report {
        base: base.to_string(),
        head,
        files,
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        "text" => {
            for f in &report.files {
                println!(
                    "== {} ({}) +{} -{} [{}]",
                    f.path, f.kind, f.added, f.removed, f.algorithm
                );
                print!("{}", f.unified);
            }
        }
        _ => anyhow::bail!("unknown format {}", format),
    }
    Ok(())
}
