//! Keep authored prose separate from host diagnostics until the display boundary.
//!
//! Transient composition only: persistent identity belongs to Observed Report's
//! canonical content address and the existing turn-outcome artifact.
#[derive(Debug, Default)]
pub(crate) struct FinalReply {
    pub(super) model: String,
    pub(super) notices: Vec<String>,
}

impl From<String> for FinalReply {
    fn from(model: String) -> Self {
        Self {
            model,
            notices: Vec::new(),
        }
    }
}

impl FinalReply {
    pub(super) fn harness(notice: String) -> Self {
        Self::default().with_notice(notice)
    }

    pub(super) fn with_notice(mut self, notice: String) -> Self {
        if !notice.trim().is_empty() {
            self.notices.push(notice);
        }
        self
    }

    pub(super) fn annotation(&mut self, annotated: String) {
        if annotated != self.model {
            // Existing gates append diagnostics. A gate that deduplicates an
            // existing annotation may rewrite its presentation; preserve the
            // original model bytes and show that checked view as harness data.
            self.notices.push(
                annotated
                    .strip_prefix(&self.model)
                    .unwrap_or(&annotated)
                    .trim()
                    .to_owned(),
            );
        }
    }

    #[cfg(test)]
    pub(super) fn test_text(&self) -> String {
        std::iter::once(self.model.as_str())
            .chain(self.notices.iter().map(String::as_str))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
