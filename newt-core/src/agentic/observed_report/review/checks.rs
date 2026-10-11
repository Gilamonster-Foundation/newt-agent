use super::{future, number, regex, Facts, Verdict};
use std::sync::LazyLock;

static COUNT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"(?i)([\d,]+)\s+(?:tests?\s+)?(passed|pass|failed|fail|ignored|total|tests?)\b")
});
static COMMAND: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"`(cargo (?:test|nextest run)[^`\n]*)`|\b(cargo (?:test|nextest run)[^\n]*?)\s+(?:—|–|:)",
    )
});
static LIBTEST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in [\d.]+s$",
    )
});
static NEXTEST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"^Summary \[[^]]+\] (\d+) tests run: (\d+) passed(?: \([^)]*\))?(?:, (\d+) failed)?(?:, (\d+) skipped)?$",
    )
});

fn totals(check: &super::super::Check) -> Option<[i64; 4]> {
    if check.truncated
        || !matches!(check.outcome.as_str(), "Passed" | "Failed")
        || !matches!(check.exit, Some(0 | 101 | 100))
        || (check.outcome == "Passed") != (check.exit == Some(0))
    {
        return None;
    }
    let inspection = agent_bridle::inspect_shell(&check.command).ok()?;
    if inspection.commands.len() != 1
        || !inspection.constructs.is_empty()
        || !inspection.commands[0].redirects.is_empty()
        || check.command.contains(['|', ';', '&'])
    {
        return None;
    }
    let mut totals = [0_i64; 4];
    let mut found = 0;
    let mut nextest = false;
    let mut finished = 0;
    for line in &check.lines {
        if line.starts_with("Finished ") {
            finished += 1;
        }
        if let Some(c) = LIBTEST.captures(line) {
            if nextest {
                return None;
            }
            let passed = number(&c[2])?;
            let failed = number(&c[3])?;
            if (c[1] == *"FAILED") != (failed > 0) {
                return None;
            }
            totals[0] = totals[0].checked_add(passed)?;
            totals[1] = totals[1].checked_add(failed)?;
            totals[2] = totals[2].checked_add(number(&c[4])?)?;
            found += 1;
        } else if let Some(c) = NEXTEST.captures(line) {
            if found != 0 {
                return None;
            }
            totals[0] = number(&c[2])?;
            totals[1] = c.get(3).map_or(Some(0), |m| number(m.as_str()))?;
            // nextest skips include filtering, not libtest ignored tests.
            totals[2] = -1;
            if totals[0].checked_add(totals[1])? != number(&c[1])? {
                return None;
            }
            nextest = true;
            found = 1;
        } else if line.starts_with("test result:") || line.starts_with("Summary ") {
            return None;
        }
    }
    if found == 0 || finished > 1 || (check.exit == Some(0)) != (totals[1] == 0) {
        return None;
    }
    totals[3] = totals[0].checked_add(totals[1])?;
    Some(totals)
}

pub(super) fn classify(clause: &str, facts: &Facts<'_>) -> Option<Verdict> {
    static CODE: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"`+[^`\n]*`+"));
    let visible = CODE.replace_all(clause, " ");
    if future(&visible) {
        return None;
    }
    if COUNT.is_match(&visible) {
        if super::quantities::approximate(&visible) || COMMAND.captures_iter(clause).count() > 1 {
            return Some(Verdict::Unverified(
                "approximate quantity or multiple named invocations",
            ));
        }
        let Some(command) = COMMAND
            .captures(clause)
            .and_then(|c| c.get(1).or_else(|| c.get(2)))
            .map(|m| m.as_str())
        else {
            return Some(Verdict::Unverified(
                "test quantity has no exact named invocation",
            ));
        };
        let check = facts
            .checks
            .iter()
            .find(|c| c.command == command && Path::new(&c.cwd) == facts.root);
        let Some(counts) = check.and_then(totals) else {
            return Some(Verdict::Unverified(
                "named invocation lacks complete scoped totals",
            ));
        };
        let mut correct = true;
        for c in COUNT.captures_iter(&visible) {
            let index = match c[2].to_lowercase().as_str() {
                "passed" | "pass" => 0,
                "failed" | "fail" => 1,
                "ignored" => 2,
                _ => 3,
            };
            if counts[index] < 0 {
                return Some(Verdict::Unverified("test category not observed"));
            }
            let Some(claimed) = number(&c[1]) else {
                return Some(Verdict::Unverified("unsupported number syntax"));
            };
            correct &= claimed == counts[index];
        }
        return Some(if correct {
            Verdict::Correct
        } else {
            let ignored = if counts[2] >= 0 {
                format!(", {} ignored", counts[2])
            } else {
                String::new()
            };
            Verdict::Corrected(format!(
                "last observed `{command}` in `{}`: {} passed, {} failed{ignored}, {} executed tests (historical)", facts.root.display(), counts[0], counts[1], counts[3]
            ))
        });
    }
    publication(clause, &visible, facts)
}
use std::path::Path;

fn publication(clause: &str, visible: &str, facts: &Facts<'_>) -> Option<Verdict> {
    static PR: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex(
            r"(?i)\b(?:PR|pull request)\b.*\b(?:created|opened|blocked|failed|open|merged)\b|\b(?:created|opened|merged)\b.*\b(?:PR|pull request)\b",
        )
    });
    static PUSH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex(r"(?i)\bpushed\b|\bpush\b.*\b(?:blocked|failed|succeeded)\b"));
    static URL: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"https://[^\s`)]+/pull/\d+"));
    let lower = visible.to_lowercase();
    let prefix = if PR.is_match(visible) {
        "PR creation observed:"
    } else if PUSH.is_match(visible) {
        "Push observed:"
    } else {
        return None;
    };
    // Backtick command examples alone are not assertions about execution.
    if clause.trim().starts_with('`') && clause.trim().ends_with('`') {
        return None;
    }
    if lower
        .split(|c: char| !c.is_alphabetic())
        .any(|w| matches!(w, "open" | "closed" | "merged" | "later" | "currently"))
    {
        return Some(Verdict::Unverified("no live PR-state observation"));
    }
    static ID: LazyLock<regex::Regex> =
        LazyLock::new(|| regex(r"(?i)\b(?:PR|pull request)\s*#(\d+)"));
    static BRANCH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex(r"(?i)\bbranch\s+`?([\w./-]+)`?"));
    static SHA: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"\b[0-9a-f]{7,40}\b"));
    static NUMBERED: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"#\d+\b"));
    if prefix == "PR creation observed:"
        && (NUMBERED.find_iter(clause).count() > 1 || URL.find_iter(clause).count() > 1)
    {
        return Some(Verdict::Unverified("multiple PR objects in one clause"));
    }
    let matches: Vec<_> = facts
        .publications
        .iter()
        .filter(|p| p.starts_with(prefix))
        .filter(|p| {
            URL.find(clause).is_none_or(|url| {
                p.strip_prefix(prefix)
                    .is_some_and(|observed| observed.trim() == url.as_str())
            })
        })
        .filter(|p| {
            ID.captures(clause)
                .is_none_or(|c| p.trim_end().ends_with(&format!("/pull/{}", &c[1])))
        })
        .filter(|p| {
            prefix != "Push observed:"
                || (BRANCH
                    .captures(clause)
                    .is_none_or(|c| p.contains(&format!("branch {},", &c[1])))
                    && SHA.find_iter(clause).all(|m| {
                        p.split("revision ")
                            .nth(1)
                            .is_some_and(|sha| sha.starts_with(m.as_str()))
                    }))
        })
        .collect();
    if matches.len() != 1 {
        return Some(Verdict::Unverified(
            "no unique matching governed publication receipt",
        ));
    }
    static NEGATIVE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex(
            r"(?i)\b(?:PR|pull request|push)(?:\s*#\d+)?\s+(?:(?:creation|was|is)\s+)*(?:blocked|failed|not)\b|\b(?:cannot|couldn't|did not|not)\s+(?:create|open|push|pushed|created)\b",
        )
    });
    let negative = NEGATIVE.is_match(&lower);
    Some(if negative {
        Verdict::Corrected(format!(
            "{} (historical receipt; later failures are separate attempts)",
            matches[0]
        ))
    } else {
        Verdict::Correct
    })
}

#[cfg(test)]
#[path = "checks_tests.rs"]
mod tests;
