//! #2959: preview the next request from an owned snapshot.
//!
//! Running the preview through the live turn loop disturbed session state one
//! field at a time (#2951, eight review rounds). The preview is instead a pure
//! function of a [`RequestSnapshot`]: owned data in, a [`RequestReport`] out.
//! This module imports no store, sink, or HTTP client, so a preview cannot
//! reach live state or the network; the type of [`RequestSnapshot::report`]
//! (`&self`, no sink) is the whole contract.

use super::request_report::{Annotation, RequestReport};
use serde_json::Value;

/// Everything a preview reads, copied out of the session. Owned, so it cannot
/// alias live state; built by the caller from shared borrows only.
#[derive(Clone, Debug)]
pub struct RequestSnapshot {
    pub system_texts: Vec<String>,
    /// `None` when the provider omits the tools field entirely.
    pub tools: Option<Vec<Value>>,
    pub notes: Vec<Annotation>,
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
        Ok(RequestReport::build_from_parts(
            &texts,
            self.tools.as_deref(),
            &self.notes,
        ))
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

    /// The preview report is byte-identical to what the send path records for
    /// the same texts and tools, so it cannot drift from the real request.
    #[test]
    fn preview_matches_the_send_path_report() {
        let snap = snapshot(&["Workspace: /w\n\nrules"]);
        let sink = ReportSink::new();
        sink.annotate("ws", "test", "Workspace: /w");
        sink.observe_request(&["Workspace: /w\n\nrules"], snap.tools.as_deref())
            .unwrap();
        assert_eq!(
            snap.report().unwrap().to_markdown(),
            sink.last_report().unwrap().to_markdown()
        );
    }
}
