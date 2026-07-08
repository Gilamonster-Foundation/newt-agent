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
//! emitter the assistant's own output uses and `println!`s the result into the
//! terminal's own scrollback — line-oriented, copy-pasteable, and identical
//! over SSH / tmux / pipe. With `color == false` (headless / `NO_COLOR` /
//! non-TTY) it degrades to the raw Markdown source, the same honest passthrough
//! as every other output path.
//!
//! **Presence.** Unlike `save_note` / `recall`, this tool needs no injected
//! capability — it only writes to the output sink every session already has —
//! so it rides [`Gate::Always`](super::tools) and is advertised in eval /
//! headless / ACP sessions too (where it prints the raw source).
//!
//! [`render_markdown`]: super::render_markdown

use std::io::{self, Write};

use super::display::{print_tool_call, term_cols};
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
     reply.";

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
pub(crate) fn execute_render_report(args: &serde_json::Value, color: bool) -> String {
    match compose_document(args) {
        Ok((markdown, ack)) => {
            // The ⚙ header line, consistent with every other tool call.
            print_tool_call("render_report", &ack, color);
            let rendered = render_markdown(
                &markdown,
                RenderOpts {
                    color,
                    cols: term_cols(),
                },
            );
            // render_markdown owns no trailing newline — the caller does.
            let mut out = io::stdout();
            let _ = writeln!(out, "{rendered}");
            let _ = out.flush();
            ack
        }
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compose(v: serde_json::Value) -> Result<(String, String), String> {
        compose_document(&v)
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
}
