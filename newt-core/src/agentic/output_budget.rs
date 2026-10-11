//! Advisory edit sizing from configured output capacity and read-authorized bytes.
use super::scheduled::{StepLedger, StepStatus};
use crate::{tokens::TokenEstimation, Scope};
use std::{collections::BTreeSet, io::Read, path::Path, sync::LazyLock};

const COPY: &str = "Move large blocks with write_file + copy_from {path, start_line, end_line} (inclusive line ranges; no retyping). Otherwise split into several cohesive extractions, one per publish cycle. This is planning advice, never a refusal.";

pub(super) fn guidance(
    cap: Option<u32>,
    ledger: Option<&dyn StepLedger>,
    root: &Path,
    read: &Scope<String>,
    estimation: TokenEstimation,
) -> String {
    let Some(cap) = cap else {
        return format!("[Harness output planning; not assistant-authored]\nPer-reply output budget: unknown (no configured provider allowance). {COPY}");
    };
    let retype = cap / 4;
    let mut note = format!("[Harness output planning; not assistant-authored]\nPer-reply output budget: {cap} tokens (configured ceiling; request admission may lower it). Keep all retyped content in one reply at most {retype} tokens (one quarter), leaving room for reasoning and tool syntax. {COPY}");
    let Some(ledger) = ledger else {
        return note;
    };
    let plan = ledger.snapshot();
    let target = plan.target.as_ref().filter(|p| p.valid());
    let mut ranges = BTreeSet::new();
    if let Some(target) = target {
        for (start, end) in line_ranges(&target.selection.scope) {
            ranges.insert((target.selection.path.clone(), start, end));
        }
    }
    static PATH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[/\\\w.-]+\.[A-Za-z][A-Za-z0-9]*").unwrap());
    for step in plan.steps.iter().filter(|s| s.status != StepStatus::Done) {
        let paths: BTreeSet<_> = PATH
            .find_iter(&step.description)
            .map(|m| m.as_str())
            .collect();
        let path = match paths.len() {
            0 => target.map(|t| t.selection.path.as_str()),
            1 => paths.first().copied(),
            _ => None, // Multiple sources: do not guess a range's owner.
        };
        if let Some(path) = path.filter(|p| valid_path(p)) {
            for (start, end) in line_ranges(&step.description) {
                ranges.insert((path.to_owned(), start, end));
            }
        }
    }
    // Bounded advisory I/O and prompt growth, with no retained source contents.
    for (path, start, end) in ranges.into_iter().take(8) {
        let label = serde_json::to_string(&path).expect("path string");
        match measured_range(&root.join(&path), read, start, end, estimation) {
            Some(tokens) if tokens > retype as usize => note.push_str(&format!(
                "\nPlanned extraction exceeds the recommended retyping budget: source (data) {label}, lines {start}-{end}, approximately {tokens} tokens of serialized content versus {retype}. Use copy_from or split into cohesive extractions, one per publish cycle."
            )),
            None => note.push_str(&format!("\nPlanned source range (data) {label}, lines {start}-{end}: size unavailable under current read authority or read limits; do not assume it fits.")),
            _ => {},
        }
    }
    note
}

fn valid_path(path: &str) -> bool {
    !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && !path.starts_with('/')
        && path.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    static RANGE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)(?:\blines?\s+|:L?|\bL)([0-9]+)\s*(?:-|–|\.\.|\bto\b|\bthrough\b)\s*L?([0-9]+)\b",
        )
        .unwrap()
    });
    RANGE
        .captures_iter(text)
        .filter_map(|c| {
            let (start, end) = (c[1].parse().ok()?, c[2].parse().ok()?);
            (start > 0 && end >= start).then_some((start, end))
        })
        .collect()
}

fn measured_range(
    path: &Path,
    read: &Scope<String>,
    start: usize,
    end: usize,
    estimation: TokenEstimation,
) -> Option<usize> {
    let mut file = super::tools::file_capture::open_for_scope(read, path, true).ok()?;
    let before = file.metadata().ok()?;
    const LIMIT: u64 = 4 * 1024 * 1024;
    if before.len() > LIMIT {
        return None;
    }
    let mut text = String::new();
    (&mut file).take(LIMIT + 1).read_to_string(&mut text).ok()?;
    let after = file.metadata().ok()?;
    if text.len() as u64 > LIMIT
        || before.len() != after.len()
        || after.len() != text.len() as u64
        || before.modified().ok() != after.modified().ok()
    {
        return None;
    }
    let selected: Vec<_> = text.lines().skip(start - 1).take(end - start + 1).collect();
    if selected.len() != end - start + 1 {
        return None;
    }
    let serialized = serde_json::to_string(&selected.join("\n")).ok()?;
    Some(estimation.tokens_for_chars(serialized.chars().count()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::scheduled::SessionStepLedger;

    /// Planning advice measures live authorized bytes and never treats an
    /// unreadable or out-of-range source as a small extraction.
    #[test]
    fn output_budget_planning_steps_refresh_and_abstain() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SessionStepLedger::default();
        let path = dir.path().join("source.rs");
        std::fs::write(&path, "long source line\n".repeat(100)).unwrap();
        ledger.set_plan(&["Extract source.rs:1-100".into()]);
        let render = |read: &Scope<String>| {
            guidance(
                Some(400),
                Some(&ledger),
                dir.path(),
                read,
                TokenEstimation::default(),
            )
        };
        assert!(render(&Scope::All).contains("Planned extraction exceeds"));
        assert!(render(&Scope::none()).contains("size unavailable"));
        std::fs::write(&path, "x\n".repeat(100)).unwrap();
        assert!(!render(&Scope::All).contains("Planned extraction exceeds"));
        ledger.set_plan(&["Extract source.rs lines 1-101".into()]);
        assert!(render(&Scope::All).contains("size unavailable"));
        ledger.advance();
        assert!(
            !render(&Scope::All).contains("Planned source"),
            "completed steps are not pending extractions"
        );
        let unknown = guidance(
            None,
            Some(&ledger),
            dir.path(),
            &Scope::All,
            TokenEstimation::default(),
        );
        assert!(unknown.contains("unknown") && unknown.contains("copy_from"));
    }

    /// Absolute, parent-traversing and ambiguous subjects cannot be rebound to
    /// similarly named local files and produce invented extraction measurements.
    #[test]
    fn output_budget_planning_does_not_rebind_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("source.rs"),
            "long source line\n".repeat(100),
        )
        .unwrap();
        let ledger = SessionStepLedger::default();
        for step in [
            "Extract /source.rs:1-100",
            "Extract ../source.rs lines 1-100",
            r"Extract C:\source.rs lines 1-100",
            "Extract source.rs and other.rs lines 1-100",
        ] {
            ledger.set_plan(&[step.into()]);
            let text = guidance(
                Some(400),
                Some(&ledger),
                dir.path(),
                &Scope::All,
                TokenEstimation::default(),
            );
            assert!(
                !text.contains("Planned extraction exceeds"),
                "{step}: {text}"
            );
        }
    }

    /// The same configured generation maximum used by Chat/Responses limits
    /// the planning ceiling even if an explicit allowance requests more.
    #[test]
    fn output_budget_planning_uses_configured_generation_maximum() {
        let _lock = crate::process_env::lock();
        let _env = crate::agentic::tools::disable_ocap_tests::EnvVar::set(
            "NEWT_GENERATION_MAX_TOKENS",
            "800",
        );
        let cap = super::super::generation_bounds::output_cap(Some(1200), None, 0).unwrap();
        let note = guidance(
            Some(cap),
            None,
            Path::new("unused"),
            &Scope::none(),
            TokenEstimation::default(),
        );
        assert!(note.contains("Per-reply output budget: 800 tokens"));
        assert!(note.contains("at most 200 tokens"));
    }
}
