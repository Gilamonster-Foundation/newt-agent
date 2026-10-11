//! Narrow factual clauses only. Unrecognized quantities are not verified prose.
mod checks;
mod evidence;
mod quantities;
mod spans;
use super::{files::Snapshot, Check};
use content_addressable::{canonical, ContentAddressable, ContentError};
pub(crate) use evidence::Evidence;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Edit {
    start: usize,
    end: usize,
    original: String,
    replacement: String,
}

/// Byte offsets refer to the disclosed draft, never to an undisclosed original.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Review {
    schema: String,
    pub(crate) original: String,
    pub(crate) source_draft: String,
    observation: Option<String>,
    pub(crate) edits: Vec<Edit>,
}
impl ContentAddressable for Review {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}
impl Review {
    pub(crate) fn rendered(&self) -> String {
        let mut text = self.original.clone();
        for edit in self.edits.iter().rev() {
            text.replace_range(edit.start..edit.end, &edit.replacement);
        }
        text
    }
    /// Used before publication as well as in inverse tests: reject a substituted
    /// draft/edit map instead of applying unverified offsets to model speech.
    pub(crate) fn restore(&self, rendered: &str) -> Option<String> {
        let mut text = String::new();
        let mut offset = 0;
        let mut tail = rendered;
        for edit in &self.edits {
            let unchanged = self.original.get(offset..edit.start)?;
            if self.original.get(edit.start..edit.end)? != edit.original {
                return None;
            }
            tail = tail
                .strip_prefix(unchanged)?
                .strip_prefix(&edit.replacement)?;
            text.push_str(unchanged);
            text.push_str(&edit.original);
            offset = edit.end;
        }
        if tail != self.original.get(offset..)? {
            return None;
        }
        text.push_str(tail);
        Some(text)
    }
}

pub(super) struct Facts<'a> {
    pub before: Option<&'a Snapshot>,
    pub after: Option<&'a Snapshot>,
    pub checks: &'a [Check],
    pub publications: &'a [String],
    pub root: &'a Path,
}

pub(super) enum ClaimAssessment {
    Correct,
    Corrected(String),
    Unverified(&'static str),
}

static LIST: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex(r"^\s*(?:[-*]|\d+[.)])\s+"));

fn marker(text: &str) -> bool {
    text.starts_with("[corrected by newt:") || text.starts_with("[unverified by newt:")
}

pub(super) fn review(draft: &str, facts: Facts<'_>) -> Review {
    let mut review = Review {
        schema: "newt.report-review/v1".into(),
        observation: None,
        original: draft.into(),
        source_draft: draft.into(),
        edits: vec![],
    };
    let mut fence = super::prose::Fence::default();
    let mut offset = 0;
    for line in draft.split_inclusive('\n') {
        let whole = line.trim_end_matches(['\r', '\n']);
        let prefix = LIST.find(whole).map_or(0, |m| m.end());
        let clause = &whole[prefix..];
        let trimmed = clause.trim_start();
        if !fence.advance(line) && !clause.starts_with("    ") && !trimmed.starts_with('>') {
            for range in spans::claims(clause) {
                let claim = &clause[range.clone()];
                let quantity = quantities::classify(claim, &facts);
                let check = checks::classify(claim, &facts);
                let verdict = match (quantity, check) {
                    (Some(_), Some(_)) => Some(ClaimAssessment::Unverified(
                        "inseparable file and check/publication claims",
                    )),
                    (quantity, check) => quantity.or(check),
                };
                let replacement = match verdict {
                    Some(ClaimAssessment::Corrected(observed)) => {
                        Some(format!("[corrected by newt: {}]", spans::escape(&observed)))
                    }
                    Some(ClaimAssessment::Unverified(reason)) => Some(format!(
                        "[unverified by newt: {} — {reason}]",
                        spans::escape(claim)
                    )),
                    _ => None,
                };
                if let Some(replacement) = replacement {
                    review.edits.push(Edit {
                        start: offset + prefix + range.start,
                        end: offset + prefix + range.end,
                        original: claim.into(),
                        replacement,
                    });
                }
            }
        }
        offset += line.len();
    }
    if !review.edits.is_empty() {
        // Escape genuine model-authored marker lines in a reviewed draft so
        // projection cannot confuse them with the host's replacements. The
        // reversible edit map records this presentation escape too.
        let mut offset = 0;
        let mut fence = super::prose::Fence::default();
        for line in draft.split_inclusive('\n') {
            let whole = line.trim_end_matches(['\r', '\n']);
            let prefix = LIST.find(whole).map_or(0, |m| m.end());
            let clause = &whole[prefix..];
            if !fence.advance(line) && !line.starts_with("    ") {
                for range in spans::authored(clause) {
                    let start = offset + prefix + range.start;
                    let end = offset + prefix + range.end;
                    if review.edits.iter().any(|e| e.start < end && start < e.end) {
                        continue;
                    }
                    let original = &clause[range];
                    review.edits.push(Edit {
                        start,
                        end,
                        original: original.into(),
                        replacement: format!("\\{original}"),
                    });
                }
            }
            offset += line.len();
        }
        review.edits.sort_by_key(|e| e.start);
    }
    review
}

/// Only called within a recognized harness report envelope. Whole reviewed
/// clauses are host presentation, not assistant speech, even when unverified.
pub(super) fn model_projection(prose: &str) -> String {
    // A plain model-authored mention of a marker is not a reviewed report.
    // Require the complete review footer as well as the outer report envelope.
    let footer = prose.lines().last().unwrap_or("");
    let reviewed = footer == "[unverified by newt: review evidence unavailable]"
        || footer
            .strip_prefix("[corrected by newt: review evidence ")
            .and_then(|s| s.strip_suffix(']'))
            .and_then(|s| {
                s.strip_prefix("spill:").or_else(|| {
                    s.strip_prefix("review ")
                        .and_then(|s| s.strip_suffix(" in prompt artifacts"))
                })
            })
            .is_some_and(|s| crate::agentic::SpillCid::parse(s).is_ok());
    if !reviewed {
        return prose.into();
    }
    let mut fence = super::prose::Fence::default();
    let mut out = String::new();
    for line in prose.split_inclusive('\n') {
        if fence.advance(line) || line.starts_with("    ") {
            out.push_str(line);
            continue;
        }
        let projected = spans::project(line);
        let prefix = LIST.find(&projected).map_or(0, |m| m.end());
        // Removing host clauses must not manufacture a list item or separator.
        if projected == line || projected[prefix..].chars().any(|c| c.is_alphanumeric()) {
            out.push_str(&projected);
        }
    }
    if out.trim().is_empty() {
        String::new()
    } else {
        out
    }
}

fn number(text: &str) -> Option<i64> {
    let text = text.replace('−', "-");
    let unsigned = text.trim_start_matches(['+', '-']);
    if unsigned.contains(',') {
        let mut groups = unsigned.split(',');
        let first = groups.next()?;
        if first.is_empty()
            || first.len() > 3
            || !first.bytes().all(|c| c.is_ascii_digit())
            || groups.any(|g| g.len() != 3 || !g.bytes().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
    }
    text.replace(',', "").parse().ok()
}
fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("fixed report claim grammar")
}
fn future(text: &str) -> bool {
    static FUTURE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex(r"(?i)\b(?:will|would|could|should|aim|target|goal|plan to|example)\b")
    });
    FUTURE.is_match(text)
}

impl super::State {
    pub(crate) fn compose(
        &mut self,
        root: &Path,
        scope: &crate::Scope<String>,
        draft: &str,
        source_draft: &str,
        evidence: Evidence<'_>,
        disclosure: Option<&crate::ocap::DisclosureFilter>,
    ) -> String {
        let report = self.render(root, scope, draft);
        let mut review = self
            .review
            .take()
            .expect("render prepares review even without facts");
        if review.edits.is_empty() {
            return format!("{report}{draft}");
        }
        review.source_draft = source_draft.into();
        review.observation = self.pinned.as_ref().map(|(cid, _)| cid.to_string());
        for edit in &mut review.edits {
            edit.replacement = crate::agentic::redact_model_facing(
                disclosure,
                std::mem::take(&mut edit.replacement),
            );
        }
        match evidence.retain(&review) {
            Ok(reference) => format!(
                "{report}{}\n\n[corrected by newt: review evidence {reference}]",
                review.rendered()
            ),
            Err(_) => {
                // Never discard bytes if retention failed. The entire draft is
                // still represented inline, with unsupported clauses labelled.
                for edit in &mut review.edits {
                    if edit.replacement.starts_with('\\') {
                        continue;
                    }
                    edit.replacement = format!(
                        "[unverified by newt: {} — review evidence unavailable]",
                        spans::escape(&edit.original)
                    );
                }
                format!(
                    "{report}{}\n\n[unverified by newt: review evidence unavailable]",
                    review.rendered()
                )
            }
        }
    }
}
