//! The `render_report` tool — present collected findings as a rendered
//! Markdown document in the plain scroller (#1004).
//!
//! **Why this exists.** A gather-and-present task ("Good morning", a triage
//! sweep, a status roll-up) ends with the model holding structured findings it
//! has no channel to *present*. The loop's normal output path styles the
//! assistant's final prose, but a doer-oriented model tends to end such a task
//! with a terse status line ("token expired, refresh it") instead of a
//! document — so the collected data never reaches the human as a report. This
//! tool is the missing affordance: the model declares "this blob is the
//! deliverable" and it is rendered as one coherent Markdown block.
//!
//! **How it honors the plain-scroller rule** (`docs/decisions/
//! plain_scroller_tui.md`). This does NOT open a pane, alternate screen, or
//! widget. It renders Markdown → ANSI through the same [`render_markdown`]
//! emitter the assistant's own output uses, then hands the document to the
//! central tool presenter for line-oriented terminal output. With
//! `color == false` (headless / `NO_COLOR` / non-TTY) it degrades to the raw
//! Markdown source, the same honest passthrough as every other output path.
//!
//! **Presence.** Unlike `save_note` / `recall`, this tool needs no injected
//! capability — it only writes to the output sink every session already has —
//! so it rides [`Gate::Always`](super::tools) and is advertised in eval /
//! headless / ACP sessions too (where it prints the raw source).
//!
//! **Trust.** Reports are model-authored display. Rendering does not verify
//! individual claims; any ledger annotation is limited to observed tool facts.
//!
//! [`render_markdown`]: super::render_markdown

use super::display::term_cols;
use super::{render_markdown, RenderOpts};

/// A section / report status. `Ok` is the clean default; `Degraded` and `Error`
/// let a report SURFACE a partial failure inline instead of aborting the whole
/// present step — the exact gap that made a morning routine end on an expired
/// JIRA token instead of rendering the 22 healthy sections. `Pending` is a
/// section-only marker for a slice still resolving (a progressive report).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Degraded,
    Error,
    Pending,
}

impl Status {
    /// Parse a top-level report status (`Pending` is section-only → rejected).
    fn from_report_key(key: &str) -> Option<Self> {
        match key {
            "ok" => Some(Self::Ok),
            "degraded" => Some(Self::Degraded),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Parse a section status (accepts `pending` on top of the report set).
    fn from_section_key(key: &str) -> Option<Self> {
        match key {
            "pending" => Some(Self::Pending),
            other => Self::from_report_key(other),
        }
    }

    /// The stable key (used in the ack string fed back to the model).
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Error => "error",
            Self::Pending => "pending",
        }
    }

    /// A heading prefix marker — empty for `Ok` (a clean heading), a glyph
    /// otherwise. Text glyphs, so they read the same with color on or off.
    fn marker(self) -> &'static str {
        match self {
            Self::Ok => "",
            Self::Degraded => "⚠",
            Self::Error => "✖",
            Self::Pending => "…",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool schema
// ---------------------------------------------------------------------------

const RENDER_REPORT_DESCRIPTION: &str =
    "Present collected findings to the user as a rendered Markdown document \
     (dashboards, triage results, status roll-ups, summaries). Use this when \
     your deliverable is information TO READ, not a status line about your \
     work — the moment you have gathered what was asked, render it. Full GFM \
     renders: headings, tables, task lists, code fences, blockquotes. A failed \
     data source is NOT a reason to abort the report: render what you have and \
     mark the affected section `degraded` or `error` (or `pending` if it is \
     still resolving) so the human sees the partial result plus exactly what is \
     missing. Prefer ONE report with `sections` over many small calls. The tool \
     result you receive back is a short ack, not the rendered text — the \
     document has already been shown to the user, so do not repeat it in your \
     reply. If your reading changes after rendering, explicitly state \
     \"Correction: [one-line reason] — supersedes the report above\" before the \
     corrected version. Reporting findings does not complete an unfinished execution request: \
     continue the requested work with tools. When the task is complete, keep the \
     final reply brief and include only new information.";

const REPORT_DELIVERY_GUIDANCE: &str =
    "The report is already visible to the user. Do not repeat its contents in \
     your reply. If the report was wrong or you have changed your reading of \
     the request, say so explicitly — start your reply with a statement such \
     as \"Correction: [one-line reason] — supersedes the report above\" — and \
     then give the corrected version. A silent second copy of the table is not \
     a correction. Continue any unfinished requested work with tools; rendering \
     a report is not completion of that work. When the task is complete, keep \
     the final reply brief and include only new information.";

/// The `render_report` tool definition. Registered `Gate::Always` (no injected
/// capability required — see the module docs), so it is advertised every
/// session.
pub fn render_report_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "render_report",
            "description": RENDER_REPORT_DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Report title — rendered as the top-level heading."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["ok", "degraded", "error"],
                        "description": "Overall status. 'degraded' = some sections failed \
                                        but the report is still useful; 'error' = the report \
                                        is materially incomplete. Defaults to 'ok'."
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional Markdown summary/intro rendered under the \
                                        title, before any sections. Use alone for a simple \
                                        one-block report."
                    },
                    "sections": {
                        "type": "array",
                        "description": "Optional detail sections, each a sub-heading. Prefer \
                                        this over many separate render_report calls.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "heading": {
                                    "type": "string",
                                    "description": "Section sub-heading."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["ok", "degraded", "error", "pending"],
                                    "description": "Per-section status marker. Use \
                                                    'degraded'/'error' for a failed data \
                                                    source, 'pending' for a slice still \
                                                    resolving. Defaults to 'ok'."
                                },
                                "body": {
                                    "type": "string",
                                    "description": "Section content as Markdown."
                                }
                            },
                            "required": ["heading", "body"]
                        }
                    }
                },
                "required": ["title"]
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Compose (pure) — split from I/O so the unit tier stays fs/term-free
// ---------------------------------------------------------------------------

/// Build the report's Markdown source and the short ack fed back to the model.
///
/// Pure: no terminal, no stdout, no clock — the [`execute_render_report`] wrapper
/// owns the rendering + printing. Returns `Err(message)` for an actionable
/// schema mistake (empty title, unknown status, a section missing its heading);
/// the message is surfaced to the model verbatim as coaching, matching the
/// `save_note` curator-error convention.
fn compose_document(args: &serde_json::Value) -> Result<(String, String), String> {
    use std::fmt::Write as _;

    let title = args["title"].as_str().unwrap_or("").trim();
    if title.is_empty() {
        return Err("render_report requires a non-empty `title`".to_string());
    }

    let status = match args["status"].as_str().map(str::trim) {
        None | Some("") => Status::Ok,
        Some(key) => Status::from_report_key(key).ok_or_else(|| {
            format!("unknown report status \"{key}\" — use \"ok\", \"degraded\", or \"error\"")
        })?,
    };

    let mut md = String::new();
    // Title heading, carrying the overall-status glyph when not ok.
    let marker = status.marker();
    if marker.is_empty() {
        let _ = writeln!(md, "# {title}");
    } else {
        let _ = writeln!(md, "# {marker} {title}");
    }
    // A one-line caption for a non-ok report, so the degrade is stated in prose
    // and not only implied by a glyph.
    match status {
        Status::Degraded => {
            let _ = writeln!(
                md,
                "\n> ⚠ Some sections are degraded — see the markers below."
            );
        }
        Status::Error => {
            let _ = writeln!(md, "\n> ✖ This report is materially incomplete.");
        }
        _ => {}
    }

    // Optional intro/summary body.
    if let Some(body) = args["body"].as_str() {
        let body = body.trim();
        if !body.is_empty() {
            let _ = writeln!(md, "\n{body}");
        }
    }

    // Optional detail sections.
    let mut section_count = 0usize;
    if let Some(sections) = args["sections"].as_array() {
        for section in sections {
            let heading = section["heading"].as_str().unwrap_or("").trim();
            if heading.is_empty() {
                return Err("each render_report section requires a non-empty `heading`".to_string());
            }
            let sstatus = match section["status"].as_str().map(str::trim) {
                None | Some("") => Status::Ok,
                Some(key) => Status::from_section_key(key).ok_or_else(|| {
                    format!(
                        "unknown section status \"{key}\" — use \"ok\", \"degraded\", \
                         \"error\", or \"pending\""
                    )
                })?,
            };
            let sbody = section["body"].as_str().unwrap_or("").trim();
            let smarker = sstatus.marker();
            if smarker.is_empty() {
                let _ = writeln!(md, "\n## {heading}");
            } else {
                let _ = writeln!(md, "\n## {smarker} {heading}");
            }
            if !sbody.is_empty() {
                let _ = writeln!(md, "\n{sbody}");
            }
            section_count += 1;
        }
    }

    let ack = if section_count > 0 {
        format!(
            "report rendered: \"{title}\" — {}, {section_count} section(s)",
            status.as_str()
        )
    } else {
        format!("report rendered: \"{title}\" — {}", status.as_str())
    };
    Ok((md, ack))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute one `render_report` call: compose the document, render it to the
/// plain scroller through the shared Markdown emitter, and return the short ack
/// the model sees (NOT the rendered text — it has already been shown, so echoing
/// it back would only burn context).
///
/// `color` is the session's color capability (false ⇒ raw-source passthrough,
/// the headless / `NO_COLOR` degrade). Errors are returned verbatim, prefixed
/// `error: ` like every tool error — an empty title or unknown status is
/// actionable coaching, not a failure to hide.
///
/// `plan_draft` is `Some` exactly when the Plan disposition is active and a
/// draft sink is wired (design: `docs/design/plan-mode-draft-present-approve.md`,
/// #2424). In that case the composed Markdown replaces the session's one
/// draft slot instead of being rendered for immediate display — the returned
/// `Option<String>` document is `None`, and the caller's own turn-end hook
/// (not this function) presents the latest revision exactly once.
pub(crate) fn execute_render_report(
    args: &serde_json::Value,
    color: bool,
    evidence: Option<&super::capability_check::Evidence>,
    plan_draft: Option<&dyn super::PlanDraftSink>,
) -> (String, Option<String>) {
    match compose_document(args) {
        Ok((markdown, ack)) => {
            // #1947: append only facts established by the turn's tool ledger.
            // Arbitrary Markdown labels are not tool identities, and rendering
            // does not independently verify report claims. Applied
            // to the MARKDOWN so the annotation renders in the operator's
            // document; `ack` (what the model sees) is left alone, because
            // this slice annotates the report rather than steering the model
            // mid-turn. `None` evidence ⇒ no recorder ⇒ nothing to check.
            let markdown = match evidence {
                Some(evidence) => super::capability_check::annotate_unsupported(markdown, evidence),
                None => markdown,
            };
            if let Some(sink) = plan_draft {
                return match sink.save_draft(markdown) {
                    Ok(revision) => (
                        format!(
                            "plan draft saved as revision {revision}; not yet shown to the \
                             operator. Finish the plan, then call exit_plan_mode."
                        ),
                        None,
                    ),
                    Err(error) => (format!("error: saving plan draft: {error}"), None),
                };
            }
            let rendered = render_markdown(
                &markdown,
                RenderOpts {
                    color,
                    cols: term_cols(),
                },
            );
            (format!("{ack}\n{REPORT_DELIVERY_GUIDANCE}"), Some(rendered))
        }
        Err(e) => (format!("error: {e}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compose(v: serde_json::Value) -> Result<(String, String), String> {
        compose_document(&v)
    }

    /// #1947 WIRING. The pure check is proven in `capability_check`; this
    /// proves `render_report` actually calls it. A check that is correct and
    /// unreachable is the exact state C2b found `RawGuard` in.
    mod capability_wiring {
        use super::*;
        use crate::agentic::capability_check::Evidence;

        const CLAIM: &str = "| converse | ✅ |\n\nVerified working end-to-end.";

        fn render(evidence: Option<&Evidence>) -> String {
            let (_ack, doc) = execute_render_report(
                &json!({"title": "Voice stack", "body": CLAIM}),
                false,
                evidence,
                None,
            );
            doc.expect("a composed report always renders a document")
        }

        fn events(names: &[(&str, bool)]) -> Vec<crate::ToolEvent> {
            names
                .iter()
                .map(|(t, ok)| crate::ToolEvent::from_call(*t, &json!({}), *ok, None))
                .collect()
        }

        #[test]
        fn ordinary_verification_table_renders_without_invented_tool_names() {
            let args = json!({
                "title": "Refactor verification",
                "sections": [{
                    "heading": "Verification gates",
                    "body": "| Gate | Result |\n|---|---|\n| cargo build --workspace | ✅ green |\n| Guard tests (16) | ✅ passed |"
                }]
            });
            let mut evidence = Evidence::default();
            evidence.record("run_command", true, Some(crate::ExecOutcome::Passed));
            let (ack, doc) = execute_render_report(&args, false, Some(&evidence), None);
            let (_, plain) = execute_render_report(&args, false, None, None);
            assert_eq!(doc, plain, "human gate labels are ordinary report content");
            assert!(ack.contains("report rendered"), "{ack}");
            assert!(
                !ack.contains("verified"),
                "rendering is not verification: {ack}"
            );
        }

        /// The absence of any tool evidence reaches the operator's document.
        #[test]
        fn a_claim_without_tool_evidence_is_annotated_in_the_rendered_document() {
            let evidence = Evidence::default();
            let doc = render(Some(&evidence));
            assert!(doc.contains("capability check (#1947)"), "{doc}");
            assert!(
                doc.contains("no tool ran in this turn"),
                "only the actual ledger fact is asserted: {doc}"
            );
        }

        /// Successful calls leave the model-authored report unmodified.
        #[test]
        fn successful_tool_history_renders_no_refutation() {
            let evidence = Evidence::from_events(&events(&[("voice__converse", true)]));
            let doc = render(Some(&evidence));
            assert!(
                !doc.contains("capability check"),
                "the ledger establishes no contradictory fact: {doc}"
            );
        }

        /// **No recorder is not an empty ledger.** Eval and headless lend no
        /// `tool_events` vec; annotating there would refute every report for
        /// a reason that has nothing to do with the report.
        #[test]
        fn no_recorder_means_no_check_rather_than_a_refutation() {
            let doc = render(None);
            assert!(
                !doc.contains("capability check"),
                "absence of a recorder must not read as absence of evidence: {doc}"
            );
        }

        /// And the ack the model sees is untouched by any of it — this slice
        /// annotates the report, it does not steer the model mid-turn.
        #[test]
        fn the_ack_is_unchanged_whatever_the_evidence_says() {
            let silent = Evidence::default();
            let (annotated, _) = execute_render_report(
                &json!({"title": "Voice stack", "body": CLAIM}),
                false,
                Some(&silent),
                None,
            );
            let (plain, _) = execute_render_report(
                &json!({"title": "Voice stack", "body": CLAIM}),
                false,
                None,
                None,
            );
            assert_eq!(annotated, plain);
        }
    }

    #[test]
    fn definition_names_the_tool_and_requires_title() {
        let def = render_report_tool_definition();
        assert_eq!(def["function"]["name"], "render_report");
        assert_eq!(def["function"]["parameters"]["required"], json!(["title"]));
    }

    #[test]
    fn body_only_report_titles_and_acks() {
        let (md, ack) = compose(json!({
            "title": "Good Morning",
            "body": "You have 3 meetings today."
        }))
        .unwrap();
        assert!(md.starts_with("# Good Morning\n"), "{md}");
        assert!(md.contains("You have 3 meetings today."), "{md}");
        assert_eq!(ack, "report rendered: \"Good Morning\" — ok");
    }

    #[test]
    fn sections_render_as_subheadings_and_are_counted() {
        let (md, ack) = compose(json!({
            "title": "Triage",
            "sections": [
                {"heading": "Calendar", "body": "2 meetings"},
                {"heading": "NVBugs", "body": "no P0s"}
            ]
        }))
        .unwrap();
        assert!(md.contains("\n## Calendar\n"), "{md}");
        assert!(md.contains("\n## NVBugs\n"), "{md}");
        assert_eq!(ack, "report rendered: \"Triage\" — ok, 2 section(s)");
    }

    #[test]
    fn degraded_section_is_marked_inline_not_aborted() {
        // The load-bearing case: one failed source, the rest still render.
        let (md, ack) = compose(json!({
            "title": "Morning",
            "status": "degraded",
            "sections": [
                {"heading": "Calendar", "body": "2 meetings"},
                {"heading": "ShadowSync deadlines", "status": "error",
                 "body": "JIRA token expired — refresh ~/.jira/token"}
            ]
        }))
        .unwrap();
        assert!(
            md.contains("# ⚠ Morning"),
            "title carries the status glyph: {md}"
        );
        assert!(md.contains("degraded"), "caption states the degrade: {md}");
        assert!(md.contains("## ✖ ShadowSync deadlines"), "{md}");
        assert!(
            md.contains("## Calendar"),
            "healthy section stays clean: {md}"
        );
        assert_eq!(ack, "report rendered: \"Morning\" — degraded, 2 section(s)");
    }

    #[test]
    fn pending_is_a_section_only_status() {
        let (md, _) = compose(json!({
            "title": "R",
            "sections": [{"heading": "Slow", "status": "pending", "body": "loading"}]
        }))
        .unwrap();
        assert!(md.contains("## … Slow"), "{md}");
        // pending at the top level is rejected.
        let err = compose(json!({"title": "R", "status": "pending"})).unwrap_err();
        assert!(err.contains("unknown report status"), "{err}");
    }

    #[test]
    fn empty_title_is_actionable_error() {
        let err = compose(json!({"title": "   "})).unwrap_err();
        assert!(err.contains("non-empty `title`"), "{err}");
    }

    #[test]
    fn unknown_status_is_actionable_error() {
        let err = compose(json!({"title": "R", "status": "critical"})).unwrap_err();
        assert!(err.contains("unknown report status"), "{err}");
        assert!(err.contains("critical"), "names the offending value: {err}");
    }

    #[test]
    fn section_without_heading_is_actionable_error() {
        let err = compose(json!({
            "title": "R",
            "sections": [{"heading": "", "body": "x"}]
        }))
        .unwrap_err();
        assert!(err.contains("non-empty `heading`"), "{err}");
    }

    /// #2424: under the Plan disposition, `render_report` replaces the
    /// session's one draft slot instead of rendering — four calls leave ONE
    /// stored draft at revision 4, and never a document to display.
    #[test]
    fn plan_disposition_stores_a_revision_instead_of_rendering() {
        use crate::agentic::PlanDraftSink as _;

        #[derive(Default)]
        struct FakeSink(std::sync::Mutex<Option<crate::agentic::PlanDraft>>);
        impl crate::agentic::PlanDraftSink for FakeSink {
            fn save_draft(&self, markdown: String) -> Result<u32, String> {
                let mut slot = self.0.lock().unwrap();
                let revision = slot.as_ref().map_or(1, |d| d.revision + 1);
                *slot = Some(crate::agentic::PlanDraft { revision, markdown });
                Ok(revision)
            }

            fn latest_draft(&self) -> Option<crate::agentic::PlanDraft> {
                self.0.lock().unwrap().clone()
            }
        }

        let sink = FakeSink::default();
        for n in 1..=4 {
            let (ack, doc) = execute_render_report(
                &json!({"title": format!("Refactor plan v{n}"), "body": "steps..."}),
                false,
                None,
                Some(&sink),
            );
            assert!(
                doc.is_none(),
                "a Plan-disposition report must never render for display: {ack}"
            );
            assert!(
                ack.contains(&format!("revision {n}")),
                "ack must name the revision it just saved: {ack}"
            );
            assert!(
                ack.contains("not yet shown"),
                "ack must say the draft has not been presented: {ack}"
            );
        }

        let latest = sink.latest_draft().expect("four saves left a draft");
        assert_eq!(
            latest.revision, 4,
            "the fourth save replaces the slot, never appends"
        );
        assert!(latest.markdown.contains("Refactor plan v4"));
        assert!(
            !latest.markdown.contains("Refactor plan v1"),
            "the draft slot holds ONE revision, not a history: {}",
            latest.markdown
        );
    }

    /// #2625 regression: the post-render nudge must give the model an explicit
    /// correction format ("Correction: … supersedes the report above") instead of
    /// an unconditional "do not repeat". Fails on main if the nudge text lacks it.
    #[test]
    fn report_delivery_nudge_permits_explicit_correction() {
        assert!(
            REPORT_DELIVERY_GUIDANCE.contains("supersedes the report above"),
            "nudge must provide the correction form the model can use: {REPORT_DELIVERY_GUIDANCE}"
        );
        assert!(
            REPORT_DELIVERY_GUIDANCE.contains("Correction:"),
            "nudge must name the prefix the model should start with: {REPORT_DELIVERY_GUIDANCE}"
        );
        // The original advisory must still be present so the model's first instinct
        // (no repeat) is unchanged when the report was right.
        assert!(
            REPORT_DELIVERY_GUIDANCE.contains("Do not repeat"),
            "nudge must still discourage silent repetition: {REPORT_DELIVERY_GUIDANCE}"
        );
        // The tool description and the delivery nudge must prescribe ONE
        // correction path, or a model following either can still produce a
        // second, unlabelled table.
        assert!(
            RENDER_REPORT_DESCRIPTION.contains("supersedes the report above"),
            "tool description must prescribe the same correction statement: {RENDER_REPORT_DESCRIPTION}"
        );
        assert!(
            !RENDER_REPORT_DESCRIPTION.contains("render_report again"),
            "tool description must not offer a second, unlabelled correction path: {RENDER_REPORT_DESCRIPTION}"
        );
    }
}
