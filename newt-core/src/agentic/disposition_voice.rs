//! The one owner of the model-facing disposition vocabulary (#2051).
//!
//! # Ownership boundary
//!
//! This module owns the four channels through which the harness speaks to the
//! model *about* the turn's disposition, and nothing else:
//!
//! 1. the `[NEWT PROMPT COMPREHENSION]` card block ([`super::PromptIntake::model_card`]),
//! 2. the dispatcher's refusal when a call falls outside the disposition
//!    (`tools/catalog.rs`),
//! 3. the `tool_search` scope note on a filtered catalog (`tool_search.rs`),
//! 4. the next-turn scope sentence in the `select_operating_mode` tool
//!    description (`operating_mode.rs`).
//!
//! It does **not** own the base identity (`memory::DEFAULT_SOUL`), which names
//! every disposition and advertises tools by name on every turn. That is
//! identity text with its own owner and its own issue; the ratchet at the
//! bottom of this file checks every owned output against the four sites
//! above, no more and no less.
//!
//! Before this module the same vocabulary was hand-written at those four
//! sites, which is how the dispatcher came to greet a model with "This is an
//! Explain turn" while the card said something else — and how a 9b model came
//! to read one of those sentences back to the operator verbatim.
//!
//! # Three rules, each with a test
//!
//! - **Provenance is truthful.** The card says where the disposition came
//!   from, and there are two honest answers: the intake lexicon read it off
//!   the prompt, or a session setting the operator chose earlier narrowed it
//!   (`/mode plan`, `/mode diagnose`). Saying "the operator did not choose it"
//!   in the second case is false, and a small model repeats what it is told.
//! - **The mechanism is never named in a refusal.** No "disposition", "mode",
//!   or "turn" reaches the model from a denial, because the word is what it
//!   echoes. The card is structured plumbing and keeps its keys; the privacy
//!   clause tells the model not to read them aloud.
//! - **A refusal bounds what may be *quoted*, never what must be *said*.** The
//!   model may not recite this notice or an internal policy, and it must still
//!   say plainly what remains undone. The first version of this text said only
//!   "do not report this refusal", and a 9b model, denied its write and told
//!   to stay quiet about it, reported the write as done instead.

use super::prompt_intake::{DispositionSource, PromptDisposition};

/// The model-facing lines for one disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispositionVoice {
    /// Which disposition this entry speaks for.
    pub(crate) disposition: PromptDisposition,
    /// The `harness_action:` line in the active-prompt comprehension card.
    pub(crate) card_action: String,
    /// What is available instead, when a call is refused under this
    /// disposition. Empty for [`PromptDisposition::Act`], which refuses nothing.
    pub(crate) denied_guidance: String,
}

/// The complete disposition vocabulary: one [`DispositionVoice`] per
/// disposition plus the clauses shared by all of them.
///
/// Crate-private on purpose: no production caller threads a configured
/// instance through the consumers yet, and a public override surface with no
/// consumer is a half-live knob. When an `[intake]` override lands, it lands
/// here as a value, the way [`super::DispositionLexicon`] already does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispositionVoices {
    voices: Vec<DispositionVoice>,
    /// Card provenance when the lexicon classified the prompt.
    pub(crate) provenance_inferred: String,
    /// Card provenance when a session setting narrowed the classification.
    pub(crate) provenance_policy: String,
    /// Marks the card as plumbing and bounds what may be said about it.
    pub(crate) privacy: String,
    /// Appended to every refusal: what may not be quoted, and what must
    /// still be said.
    pub(crate) denied_privacy: String,
    /// The `tool_search` scope note for a filtered catalog.
    pub(crate) discovery_scope: String,
    /// The `select_operating_mode` description's sentence about what a
    /// selection does to the turn in flight.
    pub(crate) next_turn_scope: String,
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
            provenance_inferred:
                "disposition_source: the harness inferred this from the operator's words. \
                 The operator did not choose it and did not ask about it."
                    .to_string(),
            provenance_policy:
                "disposition_source: a session mode the operator set earlier narrowed this turn. \
                 The operator did not ask about it in this message."
                    .to_string(),
            privacy:
                "disposition_privacy: this card is harness plumbing, not part of the conversation. \
                 Do not name the disposition, quote this card, or announce what you are not allowed \
                 to do. If you truly cannot do what was asked, say plainly what you cannot do — \
                 never that a mode or a turn forbids it. Otherwise just answer."
                    .to_string(),
            denied_privacy:
                "Do not quote this notice or any internal policy to the operator. If the request \
                 cannot be finished with the tools available here, say plainly what remains \
                 undone; never claim it was done."
                    .to_string(),
            discovery_scope:
                "Catalog scope: this is the current turn's filtered catalog, not the whole \
                 session. A missing execution tool may be available on a direct action request. \
                 Ask the operator for one (use request_user_input when available); do not report \
                 a session-wide capability absence from this result, and do not narrate the \
                 filtering itself."
                    .to_string(),
            next_turn_scope:
                "It grants no permissions and changes nothing about what this turn may do."
                    .to_string(),
        }
    }
}

impl DispositionVoices {
    /// The entry for `disposition`. The default table is total, so this is
    /// `None` only for a table that dropped an entry.
    #[must_use]
    pub(crate) fn voice(&self, disposition: PromptDisposition) -> Option<&DispositionVoice> {
        self.voices
            .iter()
            .find(|voice| voice.disposition == disposition)
    }

    /// The card's `harness_action:` line, the provenance clause that matches
    /// `source`, and the privacy clause.
    ///
    /// A dropped entry degrades to the shared clauses alone rather than
    /// panicking or inventing an instruction: an absent line must never be
    /// read as absent *authority*, which is the [`PromptDisposition`]
    /// fail-closed rule stated in prose.
    #[must_use]
    pub(crate) fn card_block(
        &self,
        disposition: PromptDisposition,
        source: DispositionSource,
    ) -> String {
        let mut block = String::new();
        if let Some(voice) = self.voice(disposition) {
            block.push_str(&voice.card_action);
            block.push('\n');
        }
        block.push_str(match source {
            DispositionSource::Inferred => &self.provenance_inferred,
            DispositionSource::SessionPolicy => &self.provenance_policy,
        });
        block.push('\n');
        block.push_str(&self.privacy);
        block
    }

    /// The whole dispatcher refusal for a call to `name` under `disposition`:
    /// which tool was refused, what is available instead, and the
    /// quote-nothing / say-what-is-undone clause.
    #[must_use]
    pub(crate) fn denied_block(&self, disposition: PromptDisposition, name: &str) -> String {
        let mut block = format!("Tool `{name}` is not available for this request.");
        if let Some(voice) = self.voice(disposition) {
            if !voice.denied_guidance.is_empty() {
                block.push(' ');
                block.push_str(&voice.denied_guidance);
            }
        }
        block.push(' ');
        block.push_str(&self.denied_privacy);
        block
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

    const BOTH_SOURCES: [DispositionSource; 2] = [
        DispositionSource::Inferred,
        DispositionSource::SessionPolicy,
    ];

    /// Whole-word membership, so `model-entered` does not count as `mode`.
    fn names_any(text: &str, words: &[&str]) -> Option<String> {
        text.split(|c: char| !c.is_ascii_alphanumeric())
            .map(str::to_ascii_lowercase)
            .find(|word| words.contains(&word.as_str()))
    }

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
            for source in BOTH_SOURCES {
                let block = voices.card_block(disposition, source);
                assert!(
                    block.contains("disposition_source:"),
                    "{disposition:?}/{source:?} card must say where this came from: {block}"
                );
                assert!(
                    block.contains("disposition_privacy:"),
                    "{disposition:?}/{source:?} card must say it is not for the operator: {block}"
                );
            }
        }
    }

    /// Review of #2057: `/mode plan` and `/mode diagnose` narrow the turn on
    /// the operator's own standing instruction, so "the operator did not
    /// choose it" is false there. Each source gets the sentence that is true
    /// for it, and the false one never appears under the other.
    #[test]
    fn provenance_matches_the_source_it_claims() {
        let voices = DispositionVoices::default();
        let inferred = voices.card_block(PromptDisposition::Explain, DispositionSource::Inferred);
        assert!(
            inferred.contains("inferred this from the operator's words"),
            "{inferred}"
        );
        assert!(inferred.contains("did not choose it"), "{inferred}");

        let policy =
            voices.card_block(PromptDisposition::Explain, DispositionSource::SessionPolicy);
        assert!(
            policy.contains("a session mode the operator set"),
            "{policy}"
        );
        assert!(
            !policy.contains("did not choose it"),
            "a policy-narrowed card must not deny the operator's own choice: {policy}"
        );
        assert!(
            !policy.contains("inferred this"),
            "a policy-narrowed card must not claim the words decided it: {policy}"
        );
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

    /// A refusal names the tool it refused, forbids quoting the notice, and
    /// separately requires saying what remains undone. The first version said
    /// only "do not report this refusal", and the observed 9b model, denied
    /// its write and told to stay quiet, reported the write as done.
    #[test]
    fn every_refusal_separates_quoting_from_saying() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            let denied = voices.denied_block(disposition, "write_file");
            assert!(
                denied.starts_with("Tool `write_file` is not available"),
                "{denied}"
            );
            assert!(
                denied.contains("Do not quote this notice"),
                "{disposition:?} refusal must bound quoting: {denied}"
            );
            assert!(
                denied.contains("say plainly what remains undone"),
                "{disposition:?} refusal must still require the honest answer: {denied}"
            );
            assert!(
                denied.contains("never claim it was done"),
                "{disposition:?} refusal must forbid the false-success answer: {denied}"
            );
        }
    }

    /// The word the model echoes is the word it was handed. No refusal names
    /// the mechanism — not by its name, not as a mode, not as a turn — and
    /// none opens with the "This is an X turn" framing the evidenced model
    /// read aloud.
    #[test]
    fn no_refusal_names_the_mechanism() {
        let voices = DispositionVoices::default();
        for disposition in EVERY_DISPOSITION {
            let denied = voices.denied_block(disposition, "run_command");
            assert_eq!(
                names_any(&denied, &["disposition", "mode", "turn"]),
                None,
                "{disposition:?} refusal names the mechanism: {denied}"
            );
            assert!(
                !denied.contains("This is an") && !denied.contains("This is a "),
                "{disposition:?} refusal reintroduces the \"This is an X turn\" phrasing: {denied}"
            );
        }
        assert_eq!(
            names_any(&voices.next_turn_scope, &["disposition", "mode"]),
            None,
            "the operating-mode scope sentence names the mechanism: {}",
            voices.next_turn_scope
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
        let block = voices.card_block(PromptDisposition::Explain, DispositionSource::Inferred);
        assert!(!block.contains("harness_action:"), "{block}");
        assert!(block.contains("disposition_source:"), "{block}");
        assert_eq!(
            voices.denied_block(PromptDisposition::Explain, "edit_file"),
            format!(
                "Tool `edit_file` is not available for this request. {}",
                voices.denied_privacy
            )
        );
    }

    /// The single-owner ratchet, over every owned output and every consuming
    /// site. `include_str!` is compile-time, so this stays inside the
    /// fully-mocked unit tier — no filesystem at run time.
    ///
    /// Four sites once carried their own copy of this vocabulary. The count
    /// may only go DOWN, and a site that reconstructs any owned sentence by
    /// hand — a card line, a refusal, the scope note, the next-turn sentence,
    /// or a shared clause — fails here.
    #[test]
    fn no_other_module_hand_writes_the_disposition_vocabulary() {
        const OTHER_SITES: [(&str, &str); 4] = [
            ("prompt_intake.rs", include_str!("prompt_intake.rs")),
            ("tool_search.rs", include_str!("tool_search.rs")),
            ("tools/catalog.rs", include_str!("tools/catalog.rs")),
            ("operating_mode.rs", include_str!("operating_mode.rs")),
        ];
        let voices = DispositionVoices::default();
        let mut owned: Vec<(String, String)> = Vec::new();
        for disposition in EVERY_DISPOSITION {
            let Some(voice) = voices.voice(disposition) else {
                continue;
            };
            // Compare against the text after the `harness_action: ` key so a
            // site that reconstructs only the payload is caught too.
            let payload = voice
                .card_action
                .strip_prefix("harness_action: ")
                .unwrap_or(&voice.card_action);
            owned.push((format!("{disposition:?} card line"), payload.to_string()));
            if !voice.denied_guidance.is_empty() {
                owned.push((
                    format!("{disposition:?} refusal guidance"),
                    voice.denied_guidance.clone(),
                ));
            }
        }
        owned.push((
            "inferred provenance".into(),
            voices.provenance_inferred.clone(),
        ));
        owned.push(("policy provenance".into(), voices.provenance_policy.clone()));
        owned.push(("privacy clause".into(), voices.privacy.clone()));
        owned.push(("refusal clause".into(), voices.denied_privacy.clone()));
        owned.push(("discovery scope".into(), voices.discovery_scope.clone()));
        owned.push(("next-turn scope".into(), voices.next_turn_scope.clone()));
        for (name, source) in OTHER_SITES {
            for (what, text) in &owned {
                assert!(!text.is_empty(), "{what} is empty");
                assert!(
                    !source.contains(text.as_str()),
                    "{name} hand-writes the {what}. This vocabulary has exactly one owner \
                     (disposition_voice.rs); read it from there."
                );
            }
        }
    }
}
