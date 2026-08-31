//! #1947: capability-claim verification for `render_report`.
//!
//! [`claim_check`](super::claim_check) verifies *path* claims and appends a
//! visible refutation for anything that does not resolve. This is its sibling
//! for *capability* claims — "verified", "working end-to-end", "tested", a
//! table of green checkmarks.
//!
//! The #1947 session: `render_report` shipped a ten-row all-✅ table and the
//! line "MCP stdio transport verified working end-to-end", off ONE
//! `tools/call` for `list_audio_devices` that touched none of the voice path.
//! STT, TTS, VAD and `converse` were never invoked, `cargo test` was never
//! run, and every daemon run in the log died on an espeak error. **Every path
//! newt cited existed, so `claim_check` was satisfied.** The claim that was
//! false was behavioral.
//!
//! # What counts as evidence
//!
//! The turn's [`ToolEvent`](crate::ToolEvent) ledger, and only that. It
//! records `{tool, args_digest, ok}` — and `args_digest` is deliberately
//! **key names plus a hash, never values**, because args carry file contents
//! and secrets. So "was `cargo test` run?" is *not* answerable here: the
//! ledger knows `run_command` was called with a `command` key and nothing
//! more. Designing against what the ledger can actually prove rather than
//! what would be convenient is the whole discipline; a check that pretended
//! to know the command would be the false claim it exists to catch.
//!
//! Two things it CAN prove, and both are what the failing session got wrong:
//!
//! 1. **Was this subject touched at all?** A status row claiming ✅ for
//!    `converse` is refutable when no ledger event names `converse` — the
//!    same shape as a cited path that does not resolve.
//! 2. **Did what ran actually succeed?** A report claiming success over a
//!    turn whose calls failed is refutable from `ok` alone.
//!
//! # Same shape as `claim_check`, not a second philosophy
//!
//! Verify what is checkable, append a visible refutation for what is not,
//! **never rewrite the model's prose**. The report the operator sees is
//! exactly what the model wrote, plus what did not check out — the same
//! choice #1941 made in preferring neutralise-visibly to reject.
//!
//! # Precision over recall
//!
//! `self_verify`'s stated bias: a spurious refutation on an honest report is
//! worse than a miss. Two consequences, both deliberate:
//!
//! * **Only an explicit per-item claim is bound to a subject.** A status
//!   table row *is* a per-item assertion, so its subject is fair to check.
//!   Free prose is not — "verified" in a sentence rarely names what it
//!   verified, and guessing would manufacture the binding.
//! * **Prose claims get the turn-level check only**, and that check fires
//!   only on facts the ledger states outright: nothing ran, or what ran
//!   failed.
//!
//! Where prose is too vague to bind and the ledger is silent, the honest
//! output is that the claim is unverifiable — which is itself information,
//! and is reported as such rather than as a refutation.
//!
//! Pure by construction: extraction is string processing and evidence is a
//! value type built from the ledger, so the unit tier stays fully mocked.

use crate::ToolEvent;

/// What the turn actually did, distilled from the ledger.
///
/// Deliberately tiny and OWNED. The alternative — lending the live
/// `Vec<ToolEvent>` down to the tool-execution site — would fight the
/// borrow the loop already holds to push into it, for no gain: nothing here
/// needs more than the names and the outcomes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// Tool names invoked this turn, first-seen order, deduplicated.
    invoked: Vec<String>,
    /// Calls whose result read as success.
    ok: usize,
    /// Calls whose result read as failure.
    failed: usize,
}

impl Evidence {
    /// Distil the ledger. Records every call, successful or not: a failed
    /// call is evidence too — it is how "every daemon run died on an espeak
    /// error" becomes checkable.
    pub(crate) fn from_events(events: &[ToolEvent]) -> Self {
        let mut out = Self::default();
        for event in events {
            if !out.invoked.iter().any(|t| t == &event.tool) {
                out.invoked.push(event.tool.clone());
            }
            if event.ok {
                out.ok += 1;
            } else {
                out.failed += 1;
            }
        }
        out
    }

    /// Nothing ran at all. The strongest refutation available: a report
    /// claiming verification over a turn that invoked no tool has nothing
    /// behind it whatsoever.
    pub(crate) fn is_silent(&self) -> bool {
        self.invoked.is_empty()
    }

    /// Whether any invoked tool name corresponds to `subject`.
    ///
    /// Matching is on normalized segments rather than equality, because the
    /// two vocabularies genuinely differ: a report says `STT` or
    /// `list audio devices` where the ledger says `voice__stt_transcribe` or
    /// `list_audio_devices`. An MCP tool arrives as `server__tool`, so the
    /// subject may legitimately match only the tail.
    fn names(&self, subject: &str) -> bool {
        let want = normalize(subject);
        if want.is_empty() {
            return false;
        }
        self.invoked.iter().any(|tool| {
            let have = normalize(tool);
            have == want || segment_contains(&have, &want) || segment_contains(&want, &have)
        })
    }
}

/// Lowercase, and every run of non-alphanumerics collapsed to one `_`, with
/// no leading or trailing separator. `"MCP stdio"` and `"mcp__stdio"` both
/// become `mcp_stdio`.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Whether `needle` appears in `haystack` on `_` boundaries — so `stt`
/// matches `voice_stt_transcribe` but not `constt`.
fn segment_contains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let padded = format!("_{haystack}_");
    padded.contains(&format!("_{needle}_"))
}

/// Vocabulary that asserts something was verified to work. Matched
/// case-insensitively against the whole report.
///
/// Kept SHORT and assertive on purpose. "should work", "expected to", "I
/// believe" are not on it: they claim nothing, and refuting a hedge would be
/// the spurious-refutation failure this module is biased against.
const VERIFICATION_PROSE: &[&str] = &[
    "verified",
    "working end-to-end",
    "working end to end",
    "end-to-end test",
    "tested and",
    "confirmed working",
    "fully tested",
    "all tests pass",
    "test suite passes",
    "smoke tested",
];

/// Markers that make a status-table cell a SUCCESS claim.
const SUCCESS_MARKERS: &[&str] = &["✅", "✔", "☑", "🟢"];

/// Whether the report asserts, in prose, that something was verified.
pub(crate) fn has_verification_prose(text: &str) -> bool {
    let lower = text.to_lowercase();
    VERIFICATION_PROSE.iter().any(|v| lower.contains(v))
}

/// The subjects of per-item success claims: the first cell of every
/// pipe-table row carrying a success marker.
///
/// A table row is an explicit assertion ABOUT a named thing, which is what
/// makes its subject fair to bind. The header and the `|---|` separator are
/// skipped, empty subjects are dropped, and order is preserved with
/// duplicates removed — the same discipline as `claim_check::path_claims`.
pub(crate) fn claimed_subjects(text: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') || !SUCCESS_MARKERS.iter().any(|m| trimmed.contains(m)) {
            continue;
        }
        let Some(first) = trimmed.trim_matches('|').split('|').next() else {
            continue;
        };
        // Strip the markdown a subject cell usually wears.
        let subject = first.trim().trim_matches(|c| "*`_ ".contains(c)).trim();
        if subject.is_empty() || SUCCESS_MARKERS.iter().any(|m| subject.contains(m)) {
            continue;
        }
        if seen.insert(subject.to_string()) {
            out.push(subject.to_string());
        }
    }
    out
}

/// The claimed subjects the ledger does not corroborate, in citation order.
pub(crate) fn unsupported_subjects(text: &str, evidence: &Evidence) -> Vec<String> {
    claimed_subjects(text)
        .into_iter()
        .filter(|s| !evidence.names(s))
        .collect()
}

/// Cap on subjects listed verbatim, matching `claim_check::LISTED_CLAIMS`.
const LISTED_SUBJECTS: usize = 8;

/// Append the capability refutation to `text` when the ledger does not
/// support what it claims; return `text` unchanged when it does, when it
/// claims nothing, or when there is nothing to say.
///
/// The original document is always preserved as an exact prefix. This
/// labels; it never rewrites.
pub(crate) fn annotate_unsupported(text: String, evidence: &Evidence) -> String {
    let unsupported = unsupported_subjects(&text, evidence);
    let prose = has_verification_prose(&text);
    let claims_anything = prose || !claimed_subjects(&text).is_empty();
    if !claims_anything {
        return text;
    }

    let mut findings: Vec<String> = Vec::new();

    // 1. Nothing ran at all — the strongest and least ambiguous refutation.
    if evidence.is_silent() {
        findings.push("no tool ran in this turn".to_string());
    } else {
        // 2. Named subjects the ledger never touched.
        if !unsupported.is_empty() {
            let listed: Vec<String> = unsupported
                .iter()
                .take(LISTED_SUBJECTS)
                .map(|s| format!("`{s}`"))
                .collect();
            let more = unsupported.len().saturating_sub(LISTED_SUBJECTS);
            let overflow = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            findings.push(format!(
                "no tool call named {}{overflow}",
                listed.join(", ")
            ));
        }
        // 3. What did run, failed. A green report over failing calls is
        //    refutable without knowing what any of them were.
        if evidence.failed > 0 {
            let total = evidence.ok + evidence.failed;
            findings.push(format!(
                "{} of {total} tool call(s) in this turn failed",
                evidence.failed
            ));
        }
    }

    if findings.is_empty() {
        return text;
    }
    format!(
        "{text}\n\n⚠ capability check (#1947): this report claims verification, but \
         the tool ledger for this turn shows {} — re-run the checks before acting on \
         the status above.",
        findings.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(tool: &str, ok: bool) -> ToolEvent {
        ToolEvent::from_call(tool, &serde_json::json!({"k": "v"}), ok, Some(1))
    }

    /// The #1947 report, as shipped: a ten-row all-✅ table plus the prose
    /// claim, over a turn that touched none of it.
    fn the_failing_report() -> String {
        let rows = [
            "list_audio_devices",
            "stt_transcribe",
            "tts_speak",
            "vad_detect",
            "converse",
            "daemon_start",
            "daemon_stop",
            "config_load",
            "audio_capture",
            "audio_playback",
        ]
        .iter()
        .map(|r| format!("| {r} | ✅ |"))
        .collect::<Vec<_>>()
        .join("\n");
        format!(
            "# Voice stack\n\n| Component | Status |\n|---|---|\n{rows}\n\n\
             MCP stdio transport verified working end-to-end.\n"
        )
    }

    /// **The scenario, refuted.** One `tools/call` that touched none of the
    /// voice path is exactly the evidence the failing session had.
    #[test]
    fn the_1947_report_is_refuted_against_the_ledger_it_actually_had() {
        let evidence = Evidence::from_events(&[event("list_audio_devices", true)]);
        let out = annotate_unsupported(the_failing_report(), &evidence);

        assert!(
            out.starts_with(&the_failing_report()),
            "the model's document must survive as an exact prefix"
        );
        assert!(out.contains("capability check (#1947)"), "{out}");
        // The nine rows nothing corroborates are named; the one that ran is
        // NOT — which is the whole point of binding to a subject.
        assert!(out.contains("`stt_transcribe`"), "{out}");
        assert!(out.contains("`converse`"), "{out}");
        assert!(
            !out.contains("`list_audio_devices`"),
            "the one subject that WAS invoked must not be refuted: {out}"
        );
        assert!(
            out.contains("(+1 more)"),
            "nine refuted, eight listed: {out}"
        );
    }

    /// **Anti-vacuous twin 1: a corroborated report passes untouched.**
    ///
    /// Without this, every assertion above would be satisfied by a function
    /// that annotated unconditionally — which would make the check noise and
    /// train the operator to ignore it.
    #[test]
    fn a_report_whose_claims_have_evidence_is_returned_unchanged() {
        let report = "# Done\n\n| Component | Status |\n|---|---|\n\
                      | stt_transcribe | ✅ |\n| converse | ✅ |\n\n\
                      Verified working end-to-end.\n"
            .to_string();
        let evidence = Evidence::from_events(&[
            event("voice__stt_transcribe", true),
            event("voice__converse", true),
        ]);
        assert_eq!(
            annotate_unsupported(report.clone(), &evidence),
            report,
            "a corroborated report must not be annotated at all"
        );
    }

    /// **Anti-vacuous twin 2: the SAME report against an empty ledger is
    /// refuted.** The pair is the proof — same input text, opposite verdict,
    /// and the only thing that changed is the evidence.
    #[test]
    fn the_same_report_against_an_empty_ledger_is_refuted() {
        let report = "# Done\n\n| Component | Status |\n|---|---|\n\
                      | stt_transcribe | ✅ |\n| converse | ✅ |\n\n\
                      Verified working end-to-end.\n"
            .to_string();
        let corroborated = Evidence::from_events(&[
            event("voice__stt_transcribe", true),
            event("voice__converse", true),
        ]);
        let silent = Evidence::default();

        assert_eq!(annotate_unsupported(report.clone(), &corroborated), report);
        let refuted = annotate_unsupported(report.clone(), &silent);
        assert_ne!(refuted, report, "an empty ledger must refute");
        assert!(refuted.contains("no tool ran in this turn"), "{refuted}");
    }

    /// A green report over a turn whose calls FAILED is refutable without
    /// knowing what any of them were — the espeak half of #1947, where every
    /// daemon run terminated on an unresolved error.
    #[test]
    fn success_claimed_over_failing_calls_is_refuted_by_outcome_alone() {
        let evidence = Evidence::from_events(&[
            event("run_command", false),
            event("run_command", false),
            event("read_file", true),
        ]);
        let out = annotate_unsupported("All green. Confirmed working.\n".to_string(), &evidence);
        assert!(
            out.contains("2 of 3 tool call(s) in this turn failed"),
            "{out}"
        );
    }

    /// **A report that claims nothing is never annotated.** The check reads
    /// every report; it must be silent on the ones making no assertion, or
    /// it is a tax on honest output rather than a check.
    #[test]
    fn a_report_making_no_claim_is_never_annotated() {
        for text in [
            "# Status\n\nI looked at the parser and it is complex.\n",
            "# Plan\n\n| Step | Owner |\n|---|---|\n| refactor | me |\n",
            // A hedge is not a claim. Refuting one would be the
            // spurious-refutation failure this module is biased against.
            "# Notes\n\nThis should work once the daemon starts.\n",
        ] {
            let evidence = Evidence::default();
            assert_eq!(
                annotate_unsupported(text.to_string(), &evidence),
                text,
                "a report claiming nothing must pass untouched: {text}"
            );
        }
    }

    /// The subject vocabularies differ on both sides, so matching is on
    /// normalized segments — and it must not match on a coincidental
    /// substring.
    #[test]
    fn a_subject_binds_across_naming_conventions_but_not_by_accident() {
        let evidence = Evidence::from_events(&[event("voice__stt_transcribe", true)]);
        for bound in [
            "stt_transcribe",
            "STT transcribe",
            "voice__stt_transcribe",
            "stt",
        ] {
            assert!(evidence.names(bound), "`{bound}` should bind");
        }
        for unbound in ["converse", "tts", "st", "transcribed", ""] {
            assert!(!evidence.names(unbound), "`{unbound}` must NOT bind");
        }
    }

    /// Table scaffolding is not a subject.
    #[test]
    fn a_header_or_separator_row_is_not_a_claim() {
        let text = "| Component | Status |\n|---|---|\n| converse | ✅ |\n";
        assert_eq!(claimed_subjects(text), vec!["converse".to_string()]);
    }

    /// A row without a success marker asserts nothing to check.
    #[test]
    fn only_a_success_marked_row_is_a_claim() {
        let text = "| a | ✅ |\n| b | ❌ |\n| c | pending |\n";
        assert_eq!(claimed_subjects(text), vec!["a".to_string()]);
    }
}
