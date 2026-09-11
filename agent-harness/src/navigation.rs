//! Legal selection is a host decision. A relevance proposal cannot relax it.
use content_addressable::ContentId;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: ContentId,
    /// Serialized message size, excluding the surrounding array punctuation.
    pub bytes: usize,
    pub required: bool,
    pub pairs: BTreeSet<String>,
}

pub fn validate_selection(
    candidates: &[Candidate],
    selected: &[ContentId],
    max_bytes: usize,
) -> crate::Result<()> {
    let ids: BTreeSet<_> = selected.iter().copied().collect();
    if ids.len() != selected.len() || ids.iter().any(|id| !candidates.iter().any(|c| c.id == *id)) {
        return Err(crate::Error::Proposal(
            "duplicate or unauthorized selection".into(),
        ));
    }
    let mut bytes = selected
        .len()
        .saturating_sub(1)
        .checked_add(2)
        .ok_or_else(|| crate::Error::Budget("selection size overflow".into()))?;
    let mut active_pairs = BTreeSet::new();
    for candidate in candidates {
        if ids.contains(&candidate.id) {
            bytes = bytes
                .checked_add(candidate.bytes)
                .ok_or_else(|| crate::Error::Budget("selection size overflow".into()))?;
            active_pairs.extend(candidate.pairs.iter());
        } else if candidate.required {
            return Err(crate::Error::Proposal(
                "selection drops pinned input or unseen result".into(),
            ));
        }
    }
    if bytes > max_bytes {
        return Err(crate::Error::Budget("projection bytes".into()));
    }
    if candidates
        .iter()
        .any(|c| !ids.contains(&c.id) && c.pairs.iter().any(|p| active_pairs.contains(p)))
    {
        return Err(crate::Error::Proposal(
            "selection splits a tool exchange".into(),
        ));
    }
    Ok(())
}
