//! The one owner of the model-facing disposition vocabulary (#2051).
//!
//! Every sentence the harness says to the model *about* the turn's disposition
//! is composed here: the active-prompt card line, the dispatcher's refusal
//! guidance, and the tool-discovery scope note. Before this module the same
//! five-way vocabulary was hand-written at four separate sites, which is how
//! the dispatcher came to greet a model with "This is an Explain turn" while
//! the card said something else entirely — and how a 9b model came to read one
//! of those sentences back to the operator verbatim.
//!
//! Two clauses are carried alongside the per-disposition lines and are what
//! #2051 was actually missing:
//!
//! - **provenance** — the disposition is the harness's own reading of the
//!   prompt. Nothing previously told the model that, so a small model read the
//!   line as an operator-imposed rule it was obliged to honour *and announce*.
//! - **privacy** — the card is plumbing and is not for the operator. Note the
//!   deliberate limit: this suppresses naming the *mechanism*, never the
//!   substance. A model that genuinely cannot do what was asked must still say
//!   so plainly; it just says what it cannot do rather than which internal
//!   mode forbade it.
//!
//! The table is a value, not a set of scattered `match` arms, so an operator
//! override (`[intake]`, the way [`super::DispositionLexicon`] is already
//! overridable) is a later data change rather than another round of edits at
//! four sites.

use super::prompt_intake::PromptDisposition;

/// The model-facing lines for one disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionVoice {
    /// Which disposition this entry speaks for. Merge and lookup are by this
    /// key, the `LanguagePack` merge-by-name convention.
    pub disposition: PromptDisposition,
    /// The `harness_action:` line in the active-prompt comprehension card.
    pub card_action: String,
    /// Dispatcher guidance when a tool is refused under this disposition.
    /// Unused for [`PromptDisposition::Act`], which refuses nothing.
    pub denied_guidance: String,
}

/// The complete disposition vocabulary: one [`DispositionVoice`] per
/// disposition plus the clauses shared by all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispositionVoices {
    voices: Vec<DispositionVoice>,
    /// Names the classification as the harness's inference.
    pub provenance: String,
    /// Marks the card as plumbing and bounds what may be said about it.
    pub privacy: String,
    /// Appended to a dispatcher refusal so a denial is not narrated either.
    pub denied_privacy: String,
    /// The `tool_search` scope note for a non-Act turn.
    pub discovery_scope: String,
}

impl Default for DispositionVoices {
    fn default() -> Self {
        Self {
            voices: vec![
                DispositionVoice {
                    disposition: PromptDisposition::Ask,
                    card_action: "harness_action: await the bounded operator clarification; do not call tools".to_string(),
                    denied_guidance: "The harness is awaiting the operator's clarification; no tool can run until they reply.".to_string(),
                },
                DispositionVoice {
                    disposition: PromptDisposition::Act,
                    card_action: "harness_action: decisions are locked; ordinary execution authority is available".to_string(),
                    denied_guidance: String::new(),
                },
                DispositionVoice {
                    disposition: PromptDisposition::Explain,
                    card_action: "harness_action: answer without mutation; bounded read/recovery tools only".to_string(),
                    denied_guidance: "Only the bounded read-only evidence and recovery tools are available here. Choose one of those, or answer directly.".to_string(),
                },
                DispositionVoice {
                    disposition: PromptDisposition::Research,
                    card_action: "harness_action: gather bounded read-only evidence; do not mutate or request capability grants".to_string(),
                    denied_guidance: "Only the bounded read-only evidence and recovery tools are available here. Capability grants, execution, mutations, and generic MCP calls need an explicit action request from the operator.".to_string(),
                },
                DispositionVoice {
                    disposition: PromptDisposition::Plan,
                    card_action: "harness_action: read evidence and maintain the harness plan ledger only; do not mutate the workspace, execute commands, or request capability grants".to_string(),
                    denied_guidance: "Reads, the harness-owned update_plan ledger, and exit from a model-entered plan phase are available here. Workspace mutations, execution, capability grants, and generic MCP calls need an explicit action request from the operator.".to_string(),
                },
            ],
            // Short, concrete, imperative. This is read by a 9b local model,
            // which is the tier newt exists to serve: it follows plain
            // sentences and fills in silence with its own narration.
            provenance:
                "disposition_source: the harness inferred this from the operator's words. \
                 The operator did not choose it and did not ask about it."
                    .to_string(),
            privacy:
                "disposition_privacy: this card is harness plumbing, not part of the conversation. \
                 Do not name the disposition, quote this card, or announce what you are not allowed \
                 to do. If you truly cannot do what was asked, say plainly what you cannot do — \
                 never that a mode or a turn forbids it. Otherwise just answer."
                    .to_string(),
            denied_privacy:
                "Do not report this refusal to the operator; choose an available tool or answer \
                 directly."
                    .to_string(),
            discovery_scope:
                "Catalog scope: this is the current turn's filtered catalog, not the whole \
                 session. A missing execution tool may be available on a direct action request. \
                 Ask the operator for one (use request_user_input when available); do not report \
                 a session-wide capability absence from this result, and do not narrate the \
                 filtering itself."
                    .to_string(),
        }
    }
}

impl DispositionVoices {
    /// The entry for `disposition`. The default table is total, so this is
    /// `None` only for a caller-supplied table that dropped an entry.
    #[must_use]
    pub fn voice(&self, disposition: PromptDisposition) -> Option<&DispositionVoice> {
        self.voices
            .iter()
            .find(|voice| voice.disposition == disposition)
    }

    /// The card's `harness_action:` line, plus the provenance and privacy
    /// clauses that keep it from reading as an operator-imposed cage.
    ///
    /// A dropped entry degrades to the shared clauses alone rather than
    /// panicking or inventing an instruction: an absent line must never be
    /// read as absent *authority*, which is the [`super::PromptDisposition`]
    /// fail-closed rule stated in prose.
    #[must_use]
    pub fn card_block(&self, disposition: PromptDisposition) -> String {
        let mut block = String::new();
        if let Some(voice) = self.voice(disposition) {
            block.push_str(&voice.card_action);
            block.push('\n');
        }
        block.push_str(&self.provenance);
        block.push('\n');
        block.push_str(&self.privacy);
        block
    }

    /// The dispatcher's refusal guidance for `disposition`, with the
    /// non-narration clause appended.
    #[must_use]
    pub fn denied_block(&self, disposition: PromptDisposition) -> String {
        let guidance = self
            .voice(disposition)
            .map(|voice| voice.denied_guidance.as_str())
            .unwrap_or_default();
        if guidance.is_empty() {
            return self.denied_privacy.clone();
        }
        format!("{guidance} {}", self.denied_privacy)
    }

    /// Replace or add entries by disposition, the merge-by-name convention
    /// [`super::DispositionLexicon`] and the language packs already use. An
    /// override that names a disposition wins; one that does not leaves the
    /// built-in entry untouched.
    pub fn merge(&mut self, overrides: impl IntoIterator<Item = DispositionVoice>) {
        for voice in overrides {
            match self
                .voices
                .iter_mut()
                .find(|existing| existing.disposition == voice.disposition)
            {
                Some(existing) => *existing = voice,
                None => self.voices.push(voice),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_DISPOSITION: [PromptDisposition; 5] = [
        PromptDisposition::Ask,
        PromptDisposition::Act,
        PromptDisposition::Explain,
        PromptDisposition::Research,
        PromptDisposition::Plan,
    ];

    #[test]
    fn the_default_table_speaks_for_every_disposition() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            assert!(
                voices.voice(disposition).is_some(),
                "no voice for {disposition:?}: a new disposition must be given one here, \
                 not left to a silent default at a call site"
            );
        }
    }

    /// #2051: the observed defect. The card said what the model must do and
    /// nothing about where the instruction came from, so a 9b model reported
    /// its compliance to the operator.
    #[test]
    fn every_card_block_states_provenance_and_privacy() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            let block = voices.card_block(disposition);
            assert!(
                block.contains("disposition_source:"),
                "{disposition:?} card must say the harness inferred this: {block}"
            );
            assert!(
                block.contains("disposition_privacy:"),
                "{disposition:?} card must say it is not for the operator: {block}"
            );
        }
    }

    /// The privacy clause suppresses the *mechanism*, never the substance. A
    /// model that cannot do the work still owes the operator a plain answer,
    /// so the clause must keep saying so.
    #[test]
    fn the_privacy_clause_still_permits_saying_what_cannot_be_done() {
        let voices = DispositionVoices::default();
        assert!(
            voices.privacy.contains("say plainly what you cannot do"),
            "privacy must not become a gag on honest limits: {}",
            voices.privacy
        );
    }

    #[test]
    fn every_refusal_carries_the_non_narration_clause() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            assert!(
                voices
                    .denied_block(disposition)
                    .contains("Do not report this refusal"),
                "{disposition:?} refusal must not invite narration"
            );
        }
    }

    /// The phrase the evidenced 9b model produced almost verbatim. It came
    /// from the dispatcher's refusal string, not the card, which is why a
    /// card-only fix would not have removed it.
    #[test]
    fn no_voice_calls_the_turn_by_its_disposition_name() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            let denied = voices.denied_block(disposition);
            assert!(
                !denied.contains("This is an") && !denied.contains("This is a "),
                "{disposition:?} refusal reintroduces the \"This is an X turn\" phrasing \
                 the model read aloud: {denied}"
            );
        }
    }

    #[test]
    fn merge_replaces_by_disposition_and_leaves_the_rest_alone() {
        let mut voices = DispositionVoices::default();
        let untouched = voices
            .voice(PromptDisposition::Plan)
            .expect("plan voice")
            .clone();
        voices.merge([DispositionVoice {
            disposition: PromptDisposition::Explain,
            card_action: "harness_action: bespoke".to_string(),
            denied_guidance: "Bespoke.".to_string(),
        }]);
        assert_eq!(
            voices
                .voice(PromptDisposition::Explain)
                .expect("explain voice")
                .card_action,
            "harness_action: bespoke"
        );
        assert_eq!(
            voices.voice(PromptDisposition::Plan).expect("plan voice"),
            &untouched,
            "an override naming one disposition must not disturb another"
        );
    }

    /// A dropped entry must fail toward *less* instruction, never toward an
    /// invented one, and must still carry the shared clauses.
    #[test]
    fn a_dropped_entry_degrades_to_the_shared_clauses() {
        let mut voices = DispositionVoices::default();
        voices
            .voices
            .retain(|voice| voice.disposition != PromptDisposition::Explain);
        let block = voices.card_block(PromptDisposition::Explain);
        assert!(!block.contains("harness_action:"), "{block}");
        assert!(block.contains("disposition_source:"), "{block}");
        assert_eq!(
            voices.denied_block(PromptDisposition::Explain),
            voices.denied_privacy
        );
    }

    /// The single-owner ratchet. `include_str!` is compile-time, so this
    /// stays inside the fully-mocked unit tier — no filesystem at run time.
    ///
    /// Four sites once carried their own copy of this vocabulary. The count
    /// may only go DOWN.
    #[test]
    fn no_other_module_hand_writes_the_disposition_vocabulary() {
        const OTHER_SITES: [(&str, &str); 3] = [
            ("prompt_intake.rs", include_str!("prompt_intake.rs")),
            ("tool_search.rs", include_str!("tool_search.rs")),
            ("tools/catalog.rs", include_str!("tools/catalog.rs")),
        ];
        let voices = DispositionVoices::default();
        for (name, source) in OTHER_SITES {
            for disposition in EVERY_DISPOSITION {
                let Some(voice) = voices.voice(disposition) else {
                    continue;
                };
                if voice.card_action.is_empty() {
                    continue;
                }
                // Compare against the text after the `harness_action: ` key so
                // a site that reconstructs only the payload is caught too.
                let payload = voice
                    .card_action
                    .strip_prefix("harness_action: ")
                    .unwrap_or(&voice.card_action);
                assert!(
                    !source.contains(payload),
                    "{name} hand-writes the {disposition:?} card line. This vocabulary has \
                     exactly one owner (disposition_voice.rs); read it from there."
                );
            }
        }
    }
}
