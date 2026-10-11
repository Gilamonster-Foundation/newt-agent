use super::{future, number, regex, ClaimAssessment, Facts};
use std::sync::LazyLock;

static PATH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"\b[\w./-]+\.(?:rs|py|js|ts|tsx|jsx|md|toml|json|yaml|yml|c|h|cpp)\b"));
static QUANTITY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(?:[\d,]+\s*[- ]\s*lines?\b|\bnet\s+[+−-]?[\d,]+|\d[\d,]*\s*(?:→|->)|\d[\d,]*\s+(?:insertions?|deletions?|files? changed))",
    )
});
static APPROX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex(r"(?i)~|≈|\b(?:about|roughly|approximately|around)\b"));
static PAIR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"(?i)([\d,]+)\s*(?:→|->)\s*([\d,]+)(?:\s+lines)?(?:\s*\(?net\s+([+−-][\d,]+)\)?)?")
});
static NET: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"(?i)\bnet\s+([+−-]?[\d,]+)"));
static SIZE: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"(?i)([\d,]+)\s*(?:-\s*)?lines?\b"));
static CHANGE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"(?i)\b(?:reduc\w*|drop\w*|decreas\w*|shrank|shorter|fewer|removed|cut|down)\b")
});
static COMPARISON: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex(r"(?i)\b(?:difference|increase\w*|grew|larger|longer|added|delta|more|less|changed)\b")
});

/// Backtick commands and identifiers are data, not factual quantities. Retain
/// file locators so `src/lib.rs` still binds a count; command claims have their
/// own parser. Equal byte lengths preserve position if this grows span parsing.
fn visible(text: &str) -> String {
    static CODE: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"`+[^`\n]*`+"));
    CODE.replace_all(text, |caps: &regex::Captures<'_>| {
        let inner = caps[0].trim_matches('`');
        if PATH.find(inner).is_some_and(|m| m.as_str() == inner) {
            inner.into()
        } else {
            " ".repeat(caps[0].len())
        }
    })
    .into_owned()
}

pub(super) fn approximate(text: &str) -> bool {
    APPROX.is_match(text)
}

pub(super) fn classify(clause: &str, facts: &Facts<'_>) -> Option<ClaimAssessment> {
    let text = visible(clause);
    if future(&text) || !QUANTITY.is_match(&text) {
        return None;
    }
    if approximate(&text) {
        return Some(ClaimAssessment::Unverified("approximate quantity"));
    }
    if text.contains("insertion") || text.contains("deletion") || text.contains("files changed") {
        return Some(ClaimAssessment::Unverified(
            "no scoped diffstat observation",
        ));
    }
    let paths: Vec<_> = PATH.find_iter(&text).map(|m| m.as_str()).collect();
    if paths.len() > 1 {
        return Some(ClaimAssessment::Unverified(
            "multiple file subjects in one clause",
        ));
    }
    let (before, after, subject) = if let Some(path) = paths.first() {
        let Some(resolved) = super::super::files::resolve(facts.before, facts.after, path) else {
            return Some(ClaimAssessment::Unverified(
                "missing or ambiguous file path",
            ));
        };
        (
            facts.before.and_then(|s| s.lines(&resolved)),
            facts.after.and_then(|s| s.lines(&resolved)),
            resolved,
        )
    } else if NET.is_match(&text) {
        if !facts
            .before
            .zip(facts.after)
            .is_some_and(|(a, b)| a.same_paths(b))
        {
            return Some(ClaimAssessment::Unverified(
                "objective file sets differ or are unavailable",
            ));
        }
        (
            facts.before.and_then(|s| s.total_lines()),
            facts.after.and_then(|s| s.total_lines()),
            "objective file total".into(),
        )
    } else {
        return Some(ClaimAssessment::Unverified("no unique file subject"));
    };
    let unverified = || {
        Some(ClaimAssessment::Unverified(
            "scoped line count unavailable or unsupported phrasing",
        ))
    };
    if let Some(pair) = PAIR.captures(&text) {
        let (Some(before), Some(after)) = (before, after) else {
            return unverified();
        };
        // An extra quantitative assertion must not be hidden by a recognized pair.
        let residue = text.replacen(&pair[0], "", 1);
        if QUANTITY.is_match(&residue) || COMPARISON.is_match(&residue) || CHANGE.is_match(&residue)
        {
            return unverified();
        }
        if number(&pair[1]).is_none()
            || number(&pair[2]).is_none()
            || pair.get(3).is_some_and(|m| number(m.as_str()).is_none())
        {
            return unverified();
        }
        let matches = number(&pair[1]) == Some(before)
            && number(&pair[2]) == Some(after)
            && pair
                .get(3)
                .is_none_or(|m| number(m.as_str()) == Some(after - before));
        return Some(if matches {
            ClaimAssessment::Correct
        } else {
            ClaimAssessment::Corrected(format!(
                "{subject}: {before} → {after} lines (net {:+})",
                after - before
            ))
        });
    }
    if let Some(net) = NET.captures(&text) {
        let residue = text.replacen(&net[0], "", 1);
        if QUANTITY.is_match(&residue) || COMPARISON.is_match(&residue) || CHANGE.is_match(&residue)
        {
            return unverified();
        }
        let (Some(before), Some(after)) = (before, after) else {
            return unverified();
        };
        return Some(compare(
            number(&net[1]),
            after - before,
            format!("{subject}: net {:+} lines", after - before),
        ));
    }
    let sizes: Vec<_> = SIZE.captures_iter(&text).collect();
    if sizes.len() != 1 {
        return unverified();
    }
    let expected = number(&sizes[0][1]);
    let Some(path) = paths.first() else {
        return unverified();
    };
    let path = regex::escape(path);
    if CHANGE.is_match(&text) {
        // Only an explicit "by N lines" reduction has defined direction.
        let reduction = regex(&format!(
            r"(?i)(?:{path}\s+(?:was\s+)?(?:reduced|dropped|shortened|shrunk)\s+by|reduced\s+{path}\s+by)\s+[\d,]+\s+lines\b"
        ));
        if !reduction.is_match(&text) {
            return unverified();
        }
        let (Some(before), Some(after)) = (before, after) else {
            return unverified();
        };
        return Some(compare(
            expected,
            before - after,
            format!("{subject}: reduction {} lines", before - after),
        ));
    }
    if COMPARISON.is_match(&text) {
        return unverified();
    }
    let absolute = regex(&format!(
        r"(?i){path}\s*(?::|is(?:\s+now)?|now|after|before)\s*:?\s*[\d,]+\s+lines\b"
    ));
    let descriptor = regex(&format!(
        r"(?i)(?:{path}\s*\([\d,]+\s+lines\)|[\d,]+-line\s+{path})"
    ));
    if !absolute.is_match(&text) {
        if descriptor.is_match(&text)
            && expected.is_some()
            && (expected == before || expected == after)
        {
            return Some(ClaimAssessment::Correct);
        }
        return unverified();
    }
    let value = if text.to_lowercase().contains("before") {
        before
    } else {
        after
    };
    let Some(value) = value else {
        return unverified();
    };
    Some(compare(
        expected,
        value,
        format!("{subject}: {value} lines"),
    ))
}
fn compare(claimed: Option<i64>, actual: i64, observed: String) -> ClaimAssessment {
    if claimed.is_none() {
        return ClaimAssessment::Unverified("unsupported number syntax");
    }
    if claimed == Some(actual) {
        ClaimAssessment::Correct
    } else {
        ClaimAssessment::Corrected(observed)
    }
}
