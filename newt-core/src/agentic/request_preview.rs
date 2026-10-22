//! #2959: preview the next request from an owned snapshot.
//!
//! Running the preview through the live turn loop disturbed session state one
//! field at a time (#2951, eight review rounds). The preview is instead a pure
//! function of a [`RequestSnapshot`]: owned data in, a [`RequestReport`] out.
//! This module imports no store, sink, or HTTP client, so a preview cannot
//! reach live state or the network; the type of [`RequestSnapshot::report`]
//! (`&self`, no sink) is the whole contract.

use super::prompt_intake::PromptIntake;
use super::prompt_read::{self, PromptReadContext};
use super::request_report::{Annotation, RequestReport};
use super::tools::ExposureSettings;
use crate::{BackendKind, OpenAiApi};
use serde_json::Value;

/// The protected active-prompt card as the next turn would install it, built
/// by the wire loops' own renderer. The prompt addresses are placeholders (no
/// receipt exists before the operator submits), and `intake` is a stand-in for
/// the unseen task; everything else is the real card.
pub fn active_prompt_card_preview(exposure: &ExposureSettings, intake: &PromptIntake) -> String {
    let base = prompt_read::active_prompt_card(PromptReadContext::new(None, "", None), exposure);
    prompt_read::active_prompt_card_with_intake(base, intake)
}

/// The tools array in the shape the wire's body builder sends: Anthropic and
/// Responses get their converted schemas, and Responses omits the field when
/// the set is empty (`None`), exactly as its `build_body` does.
pub fn wire_tools(kind: BackendKind, api: OpenAiApi, tools: &Value) -> Option<Vec<Value>> {
    match (kind, api) {
        (BackendKind::Anthropic, _) => {
            Some(super::anthropic_wire::tools_to_anthropic(tools)).filter(|t| !t.is_empty())
        }
        (BackendKind::Openai, OpenAiApi::Responses) => {
            Some(super::tools_to_responses(tools)).filter(|t| !t.is_empty())
        }
        _ => Some(tools.as_array().cloned().unwrap_or_default()),
    }
}

/// Everything a preview reads, copied out of the session. Owned, so it cannot
/// alias live state; built by the caller from shared borrows only.
#[derive(Clone, Debug)]
pub struct RequestSnapshot {
    pub system_texts: Vec<String>,
    /// `None` when the provider omits the tools field entirely.
    pub tools: Option<Vec<Value>>,
    pub notes: Vec<Annotation>,
    /// What this snapshot knowingly leaves out; printed in the report header
    /// and marks the totals partial. Never empty: a preview is never "as sent".
    pub omitted: String,
}

/// A preview needs at least one system message; nothing else can fail.
#[derive(Debug, PartialEq, Eq)]
pub struct EmptyPreview;

impl std::fmt::Display for EmptyPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "nothing to preview: the snapshot has no system message")
    }
}
impl std::error::Error for EmptyPreview {}

impl RequestSnapshot {
    /// The same report the wire loops build at send time
    /// ([`RequestReport::build_from_parts`]), without sending anything.
    pub fn report(&self) -> Result<RequestReport, EmptyPreview> {
        if self.system_texts.is_empty() {
            return Err(EmptyPreview);
        }
        let texts: Vec<&str> = self.system_texts.iter().map(String::as_str).collect();
        let mut report =
            RequestReport::build_from_parts(&texts, self.tools.as_deref(), &self.notes);
        report.partial = Some(self.omitted.clone());
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::request_report::ReportSink;
    use serde_json::json;
    use wiremock::{matchers::any, Mock, MockServer, ResponseTemplate};

    fn snapshot(system: &[&str]) -> RequestSnapshot {
        RequestSnapshot {
            system_texts: system.iter().map(|s| s.to_string()).collect(),
            tools: Some(vec![json!({"type":"function","function":{"name":"t"}})]),
            notes: vec![Annotation {
                label: "ws".into(),
                source: "test".into(),
                text: "Workspace: /w".into(),
            }],
            omitted: "test".into(),
        }
    }

    /// #2959: a preview, successful or failed, leaves the session's report
    /// sink exactly as it was (last report, notes, armed capture) and sends no
    /// inference request. There is no cancel case: `turn_cancel` is only
    /// observed inside the interrupt watch around a dispatched turn, and the
    /// TUI handler runs on the input loop between turns, outside it. The
    /// builder is also synchronous and is handed neither a sink nor a cancel
    /// flag, so no abort can reach it.
    #[tokio::test]
    async fn preview_leaves_session_state_untouched_and_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let sink = ReportSink::new();
        sink.annotate("prior", "turn", "earlier turn text");
        sink.observe_request(&["earlier turn text"], None).unwrap();
        sink.arm_show_next();
        let before = (
            sink.last_report().unwrap().to_markdown(),
            format!("{:?}", sink.snapshot_notes()),
            format!("{sink:?}"),
        );

        let ok = snapshot(&["Workspace: /w\n\nrules"]).report().unwrap();
        assert_eq!(ok.system[0].components[0].label, "ws");
        assert_eq!(ok.tools.len(), 1);
        assert_eq!(snapshot(&[]).report().unwrap_err(), EmptyPreview);

        let after = (
            sink.last_report().unwrap().to_markdown(),
            format!("{:?}", sink.snapshot_notes()),
            format!("{sink:?}"),
        );
        assert_eq!(before, after);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// The preview report matches what the send path records for the same
    /// texts and tools in every component and schema, so it cannot drift from
    /// the real request. Only the preview wording differs: the header, the
    /// `previewed` labels and the `(partial)` total.
    #[test]
    fn preview_matches_the_send_path_report() {
        let snap = snapshot(&["Workspace: /w\n\nrules"]);
        let sink = ReportSink::new();
        sink.annotate("ws", "test", "Workspace: /w");
        sink.observe_request(&["Workspace: /w\n\nrules"], snap.tools.as_deref())
            .unwrap();
        let preview = snap.report().unwrap();
        let sent = sink.last_report().unwrap();
        assert_eq!(preview.system.len(), sent.system.len());
        assert_eq!(
            format!("{:?}{:?}", preview.system, preview.tools),
            format!("{:?}{:?}", sent.system, sent.tools)
        );
        let body = |md: String| {
            md.split_once("\n## System message 1 (")
                .unwrap()
                .1
                .to_string()
        };
        assert_eq!(
            body(preview.to_markdown())
                .replace("previewed", "as sent")
                .replace("Total (partial)", "Total"),
            body(sent.to_markdown())
        );
        assert!(preview.to_markdown().contains("preview, partial"));
        assert!(!preview.to_markdown().contains("as sent"));
    }

    /// Finding (#2985 r1): a Responses request omits the tools field when the
    /// exposed set is empty, and sends the flat Responses schema otherwise.
    #[test]
    fn wire_tools_follow_each_provider_body_builder() {
        let chat =
            json!([{"type":"function","function":{"name":"t","description":"d","parameters":{}}}]);
        let get = |k, a, v: &Value| wire_tools(k, a, v);
        assert_eq!(
            get(BackendKind::Openai, OpenAiApi::Responses, &json!([])),
            None
        );
        let responses = get(BackendKind::Openai, OpenAiApi::Responses, &chat).unwrap();
        assert_eq!(responses[0]["name"], "t");
        assert!(responses[0].get("function").is_none());
        assert_eq!(
            get(BackendKind::Openai, OpenAiApi::ChatCompletions, &chat).unwrap(),
            chat.as_array().unwrap().clone()
        );
        assert_eq!(
            get(BackendKind::Openai, OpenAiApi::ChatCompletions, &json!([])),
            Some(vec![])
        );
        let anthropic = get(BackendKind::Anthropic, OpenAiApi::ChatCompletions, &chat).unwrap();
        assert_eq!(anthropic[0]["name"], "t");
        assert!(anthropic[0].get("input_schema").is_some());
        // Anthropic's body builder omits an empty tools field too.
        assert_eq!(
            get(
                BackendKind::Anthropic,
                OpenAiApi::ChatCompletions,
                &json!([])
            ),
            None
        );
    }
}
