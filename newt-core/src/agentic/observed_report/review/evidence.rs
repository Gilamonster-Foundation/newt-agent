//! Reuse prompt artifacts and session spills, never a parallel review database.
use super::Review;
use crate::agentic::{
    artifact_read::{ArtifactReadContext, PromptArtifactSink},
    content_spill::{SpillCid, SpillProvenance, SpillStore},
};
use content_addressable::ContentAddressable;

#[derive(Clone, Copy, Default)]
pub(crate) struct Evidence<'a> {
    pub sink: Option<&'a dyn PromptArtifactSink>,
    pub context: Option<ArtifactReadContext<'a>>,
    pub spill: Option<&'a dyn SpillStore>,
}
impl Evidence<'_> {
    pub(super) fn retain(&self, review: &Review) -> anyhow::Result<String> {
        anyhow::ensure!(
            review.restore(&review.rendered()).as_deref() == Some(&review.original),
            "non-invertible report review"
        );
        let cid = review.content_id()?.to_string();
        let body = serde_json::to_string(review)?;
        if let (Some(sink), Some(context)) = (self.sink, self.context) {
            // Chunk only at UTF-8 boundaries. Every append is already linked to
            // the same submitted/root receipt; the full canonical CID binds order.
            let mut rest = body.as_str();
            let mut part = 0;
            while !rest.is_empty() {
                let mut end = rest.len().min(crate::MAX_ARTIFACT_BODY_BYTES);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                let chunk = &rest[..end];
                let artifact = crate::artifact::NewPromptArtifact::new(
                    crate::artifact::ArtifactKind::TurnOutcome,
                    crate::artifact::ArtifactRelation::DerivedFrom,
                ).with_metadata(serde_json::json!({"schema":"newt.report-review/v1", "review_cid":cid, "part":part, "bytes":body.len(), "last":end == rest.len()}))
                    .with_body(chunk);
                let stored = crate::agentic::artifact_hooks::append(sink, context, artifact)?;
                anyhow::ensure!(
                    stored.body.as_deref() == Some(chunk) && stored.metadata["review_cid"] == cid,
                    "review artifact changed during retention"
                );
                rest = &rest[end..];
                part += 1;
            }
            return Ok(format!("review {cid} in prompt artifacts"));
        }
        let store = self
            .spill
            .ok_or_else(|| anyhow::anyhow!("no session evidence sink"))?;
        let staged = store
            .stage(SpillProvenance::ReportReview, body)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let handle = *staged.cid();
        store
            .commit_batch(&[staged])
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        // Check retrieval/integrity on the production path before replacing prose.
        let stored = store
            .fetch(&handle)
            .ok_or_else(|| anyhow::anyhow!("review missing after store"))?;
        anyhow::ensure!(
            SpillCid::of(&stored)? == handle,
            "review spill integrity mismatch"
        );
        let decoded: Review = serde_json::from_str(&stored.redacted_text)?;
        anyhow::ensure!(
            decoded.content_id()?.to_string() == cid,
            "review content mismatch"
        );
        anyhow::ensure!(
            decoded.restore(&review.rendered()).as_deref() == Some(&review.original),
            "review inverse mismatch"
        );
        Ok(format!("spill:{handle}"))
    }
}
