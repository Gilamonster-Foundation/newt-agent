//! Closing PR claims are grounded only in governed creation outcomes.
use super::*;
use crate::agentic::claim_check;

fn words(clause: &str) -> Vec<String> {
    clause
        .split(|c: char| c.is_whitespace() || "()[]<>`*,;\"".contains(c))
        .filter(|word| !word.is_empty())
        .map(|word| {
            word.trim_end_matches(['.', '!', '?', ':'])
                .to_ascii_lowercase()
        })
        .collect()
}

fn is_creation_claim(clause: &str) -> bool {
    let words = words(clause);
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
        return false;
    }
    words.iter().enumerate().any(|(index, word)| {
        let noun = word == "pr" || (word == "request" && index > 0 && words[index - 1] == "pull");
        if !noun {
            return false;
        }
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
        let after = &words[index + 1..];
        let opened_after = after.iter().enumerate().any(|(verb, word)| {
            matches!(word.as_str(), "open" | "opened" | "created")
                && after[..verb].iter().all(|word| {
                    let number = word.trim_start_matches('#');
                    matches!(
                        word.as_str(),
                        "is" | "was" | "has" | "been" | "now" | "already" | "successfully"
                    ) || (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
                        || crate::git_staging::validate_pr_url(word).is_some()
                })
        });
        opened_before || opened_after
    })
}

/// Number and URL references are locators; the receipt is the trusted fact.
fn references(clause: &str) -> Vec<String> {
    let tokens: Vec<_> = clause
        .split(|c: char| c.is_whitespace() || "()[]<>`*,;\"".contains(c))
        .filter(|token| !token.is_empty())
        .map(|token| token.trim_matches(['.', '!', '?', ':']))
        .collect();
    tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            if token.contains("://") && token.contains("/pull/") {
                return Some((*token).to_string());
            }
            let number = token.strip_prefix('#').unwrap_or(token);
            let previous = index.checked_sub(1).and_then(|i| tokens.get(i))?;
            (!number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
                && (previous.eq_ignore_ascii_case("pr")
                    || previous.eq_ignore_ascii_case("request")))
            .then(|| format!("#{number}"))
        })
        .collect()
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
                    if is_creation_claim(&clause) {
                        claims.push(references(&clause));
                    }
                    clause.clear();
                }
            }
            if is_creation_claim(&clause) {
                claims.push(references(&clause));
            }
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
