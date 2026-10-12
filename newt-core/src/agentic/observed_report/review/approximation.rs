//! Approximate absolute sizes: inclusive 1/divisor of an observed magnitude.
//! The grader reads the same packaged scalar; never round a count into equality.
use super::*;

pub(super) fn classify(text: &str, facts: &Facts<'_>) -> ClaimAssessment {
    let unverified = ClaimAssessment::Unverified("approximate quantity");
    // Preserve conservative handling of mixed quantities and comparisons.
    static QUALIFIED_NUMBER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex(r"(?i)(?:~|≈|\b(?:about|roughly|approximately|around)\b)\s*([+−-]?\d)")
    });
    if !QUALIFIED_NUMBER.is_match(text) {
        return unverified;
    }
    let text = QUALIFIED_NUMBER.replace_all(text, "$1");
    if CHANGE.is_match(&text)
        || COMPARISON.is_match(&text)
        || NET.is_match(&text)
        || PAIR.is_match(&text)
    {
        return unverified;
    }
    let paths: Vec<_> = PATH.find_iter(&text).map(|m| m.as_str()).collect();
    let sizes: Vec<_> = SIZE.captures_iter(&text).collect();
    if paths.len() != 1 || sizes.len() != 1 {
        return unverified;
    }
    let Some(expected) = number(&sizes[0][1]) else {
        return unverified;
    };
    let path = regex::escape(paths[0]);
    let absolute = regex(&format!(
        r"(?i){path}\s*(?::|is(?:\s+now)?|now|after|before)\s*:?\s*[\d,]+\s+lines\b"
    ));
    let descriptor = regex(&format!(r"(?i){path}\s*\(\s*[\d,]+\s+lines\)"));
    if !absolute.is_match(&text) && !descriptor.is_match(&text) {
        return unverified;
    }
    // The matched size cannot authorize another quantitative assertion.
    let residue = text.replacen(&sizes[0][0], "", 1);
    if QUANTITY.is_match(&residue) {
        return unverified;
    }
    let Some(subject) = super::super::super::files::resolve(facts.before, facts.after, paths[0])
    else {
        return unverified;
    };
    let before = facts.before.and_then(|s| s.lines(&subject));
    let after = facts.after.and_then(|s| s.lines(&subject));
    static BEFORE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex(r"(?i)\b(?:before|original|originally)\b"));
    static AFTER: LazyLock<regex::Regex> = LazyLock::new(|| regex(r"(?i)\b(?:after|now)\b"));
    let scope = text.replacen(paths[0], "", 1);
    let original = BEFORE.is_match(&scope);
    let current = AFTER.is_match(&scope) || (!original && regex(r"(?i)\bis\b").is_match(&scope));
    if original && current {
        return unverified;
    }
    let values: Vec<_> = if original {
        before.into_iter().collect()
    } else if current {
        after.into_iter().collect()
    } else {
        before.into_iter().chain(after).collect()
    };
    if values.is_empty() {
        return unverified;
    }
    if values.iter().any(|&actual| {
        (i128::from(expected) - i128::from(actual)).abs() * *super::super::APPROXIMATION_DIVISOR
            <= i128::from(actual).abs()
    }) {
        // Verify the approximate statement, leaving its qualifier in place.
        return ClaimAssessment::Correct;
    }
    let observed = if original {
        format!("{subject}: {} lines before", values[0])
    } else if !current {
        match (before, after) {
            (Some(before), Some(after)) if before != after => {
                format!("{subject}: {before} → {after} lines")
            }
            _ => format!("{subject}: {} lines", values[0]),
        }
    } else {
        format!("{subject}: {} lines", values[0])
    };
    ClaimAssessment::Corrected(observed)
}
