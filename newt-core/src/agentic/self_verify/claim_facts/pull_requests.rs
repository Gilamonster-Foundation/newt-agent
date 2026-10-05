//! Closing PR claims are grounded only in governed creation outcomes.
use super::*;
use crate::agentic::claim_check;

fn tokens(clause: &str) -> Vec<&str> {
    clause
        .split_inclusive(',')
        .flat_map(|part| part.split(|c: char| c.is_whitespace() || "()[]<>`*;\"".contains(c)))
        .map(|word| word.trim_matches(['.', '!', '?', ':']))
        .filter(|word| !word.is_empty())
        .collect()
}

fn reference(token: &str) -> Option<String> {
    let token = token.trim_end_matches(',');
    if token.contains("://") && token.contains("/pull/") {
        return Some(token.to_string());
    }
    let number = token.strip_prefix('#').unwrap_or(token);
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then(|| format!("#{number}"))
}

fn predicate_word(word: &str) -> bool {
    matches!(
        word,
        "is" | "was" | "has" | "been" | "now" | "already" | "successfully"
    )
}

/// Keep each PR's number and URL together, splitting explicit reference lists
/// and stopping before comparison/title prose. The consumed prefix also
/// identifies an immediately coordinated PR noun.
fn reference_prefix(tokens: &[&str]) -> (Vec<Vec<String>>, usize) {
    let mut groups = vec![Vec::new()];
    let mut consumed = 0;
    let mut separated = false;
    while let Some(token) = tokens.get(consumed) {
        if let Some(reference) = reference(token) {
            if separated && !groups.last().expect("one group").is_empty() {
                groups.push(Vec::new());
            }
            groups.last_mut().expect("one group").push(reference);
            separated = token.ends_with(',');
        } else {
            let word = token.to_ascii_lowercase();
            if matches!(word.as_str(), "and" | "&")
                && tokens
                    .get(consumed + 1)
                    .is_some_and(|next| reference(next).is_some())
            {
                separated = true;
            } else if !predicate_word(&word)
                && !matches!(word.as_str(), "open" | "opened" | "created")
            {
                break;
            }
        }
        consumed += 1;
    }
    (groups, consumed)
}

/// Each supported created object gets its own reference group. Merely naming
/// another PR in comparison prose does not claim that it was created too.
fn created_references(clause: &str) -> Vec<Vec<String>> {
    let tokens = tokens(clause);
    let words: Vec<_> = tokens
        .iter()
        .map(|word| word.trim_end_matches(',').to_ascii_lowercase())
        .collect();
    if words.iter().any(|word| {
        matches!(
            word.as_str(),
            "not"
                | "no"
                | "never"
                | "will"
                | "would"
                | "should"
                | "could"
                | "must"
                | "pending"
                | "if"
                | "until"
                | "previously"
                | "earlier"
        ) || word.contains("n't")
    }) {
        return Vec::new();
    }
    let nouns: Vec<_> = words
        .iter()
        .enumerate()
        .filter_map(|(index, word)| {
            (word == "pr" || (word == "request" && index > 0 && words[index - 1] == "pull"))
                .then_some(index)
        })
        .collect();
    let noun_start = |index: usize| {
        if words[index] == "request" {
            index - 1
        } else {
            index
        }
    };
    let mut claims = Vec::new();
    let mut previous: Option<(bool, usize)> = None;
    for (position, &index) in nouns.iter().enumerate() {
        let end = nouns
            .get(position + 1)
            .map_or(words.len(), |&next| noun_start(next));
        let before = &words[..index];
        let opened_before = before
            .iter()
            .rposition(|word| matches!(word.as_str(), "opened" | "created"))
            .is_some_and(|verb| {
                before[verb + 1..].iter().all(|word| {
                    matches!(
                        word.as_str(),
                        "a" | "the" | "new" | "draft" | "pull" | "successfully"
                    )
                })
            });
        let after = &words[index + 1..end];
        let opened_after = after.iter().enumerate().any(|(verb, word)| {
            matches!(word.as_str(), "open" | "opened" | "created")
                && after[..verb]
                    .iter()
                    .all(|word| predicate_word(word) || reference(word).is_some())
        });
        let coordinated = previous.is_some_and(|(created, end)| {
            created
                && words[end..noun_start(index)]
                    .iter()
                    .all(|word| matches!(word.as_str(), "and" | "&"))
        });
        let created = opened_before || opened_after || coordinated;
        let (refs, consumed) = reference_prefix(&tokens[index + 1..end]);
        if created {
            claims.extend(refs);
        }
        previous = Some((created, index + 1 + consumed));
    }
    claims
}

impl VerificationLedger {
    pub(crate) fn record_pr_outcome(&mut self, outcome: &crate::git_staging::Outcome) {
        if let crate::git_staging::Outcome::PrCreated { url } = outcome {
            if let Some(url) = crate::git_staging::validate_pr_url(url) {
                if !self.created_prs.contains(&url) {
                    self.created_prs.push(url);
                }
            }
        }
    }

    pub(crate) fn annotate_pr_claim(&self, mut text: String) -> String {
        let mut claims = Vec::new();
        for line in claim_check::asserted_claim_lines(&text) {
            let mut clause = String::new();
            for token in line.split_whitespace() {
                clause.push_str(token);
                clause.push(' ');
                if claim_check::ends_a_clause(token) {
                    claims.extend(created_references(&clause));
                    clause.clear();
                }
            }
            claims.extend(created_references(&clause));
        }
        if claims.is_empty() {
            return text;
        }
        if self.created_prs.is_empty() {
            text.push_str("\n\n⚠ claim check (#2741): Unverified PR creation claim; no governed PR creation was observed this turn.");
        } else if claims.iter().any(|refs| {
            !self.created_prs.iter().any(|url| {
                refs.iter().all(|reference| {
                    reference == url
                        || reference
                            .strip_prefix('#')
                            .is_some_and(|number| url.rsplit('/').next() == Some(number))
                })
            })
        }) {
            text.push_str(&format!("\n\n⚠ claim check (#2741): Refuted PR number or URL; governed PR creation observed this turn: {}.", self.created_prs.iter().take(8).map(|url| format!("`{url}`")).collect::<Vec<_>>().join(", ")));
        }
        text
    }
}

#[cfg(test)]
mod tests;
