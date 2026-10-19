//! #2937: the final request, made visible. Measurement only — nothing here
//! changes or prunes what is sent.
//!
//! Each wire loop calls [`ReportSink::observe_request`] with the exact system
//! texts and tools it is about to send, so the report is read off the real
//! request and cannot drift from it. Assemblers may [`ReportSink::annotate`]
//! the pieces they join into a system message (label + source); the report
//! locates each piece in the sent text and reports every remaining byte as an
//! explicit `(unattributed)` component, so the component sizes always sum to
//! the exact message length.
//!
//! Sizes are characters. Token counts are not shown: the backend reports
//! usage only after a request, and the loop's pre-send figure is an estimate.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

/// A named piece of a system message, registered by whoever assembled it.
#[derive(Clone, Debug)]
pub struct Annotation {
    pub label: String,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub label: String,
    pub source: String,
    pub chars: usize,
}

#[derive(Clone, Debug)]
pub struct SystemMessage {
    pub text: String,
    pub components: Vec<Component>,
}

#[derive(Clone, Debug)]
pub struct ToolSize {
    pub name: String,
    pub chars: usize,
    /// The full provider-format schema, exactly as dispatched.
    pub schema: Value,
}

#[derive(Clone, Debug)]
pub struct RequestReport {
    pub system: Vec<SystemMessage>,
    pub tools: Vec<ToolSize>,
    /// Length of the serialized `tools` array; exceeds the sum of `tools` by
    /// the array's brackets and commas. Zero when the provider omitted the
    /// tools key entirely (distinguished from `Some(&[])` which serialises as
    /// `[]`, 2 chars).
    pub tools_array_chars: usize,
    /// True when the provider sent no tools field at all (distinct from an
    /// empty `[]` array).
    pub tools_omitted: bool,
}

const UNATTRIBUTED: &str = "(unattributed)";

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn components_of(text: &str, notes: &[Annotation]) -> Vec<Component> {
    let mut found: Vec<(usize, usize, &Annotation)> = notes
        .iter()
        .filter(|n| !n.text.is_empty())
        .filter_map(|n| text.find(&n.text).map(|at| (at, n.text.len(), n)))
        .collect();
    // Earliest first; at equal starts the longer piece wins.
    found.sort_by_key(|&(at, len, _)| (at, std::cmp::Reverse(len)));
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut push = |label: &str, source: &str, piece: &str| {
        out.push(Component {
            label: label.into(),
            source: source.into(),
            chars: chars(piece),
        });
    };
    for (at, len, note) in found {
        if at < cursor {
            continue; // overlaps a piece already placed
        }
        if at > cursor {
            push(
                UNATTRIBUTED,
                "separators and unlabelled text",
                &text[cursor..at],
            );
        }
        push(&note.label, &note.source, &text[at..at + len]);
        cursor = at + len;
    }
    if cursor < text.len() || text.is_empty() {
        push(
            UNATTRIBUTED,
            "separators and unlabelled text",
            &text[cursor..],
        );
    }
    out
}

impl RequestReport {
    /// Build from raw system texts and tools (provider format). Preferred over
    /// `build` when the caller already has provider-specific values.
    ///
    /// `tools = None` means the provider omitted the tools field entirely
    /// (e.g. a cap-exit summary); `tools = Some(&[])` means an explicit empty
    /// array was sent.
    pub fn build_from_parts(
        system_texts: &[&str],
        tools: Option<&[Value]>,
        notes: &[Annotation],
    ) -> Self {
        let card = Annotation {
            label: "active-prompt card".into(),
            source: "agentic::prompt_read::active_prompt_card".into(),
            text: String::new(),
        };
        let system = system_texts
            .iter()
            .map(|text| {
                let mut ns = notes.to_vec();
                if text.starts_with(super::prompt_read::ACTIVE_PROMPT_PREFIX) {
                    ns.push(Annotation {
                        text: text.to_string(),
                        ..card.clone()
                    });
                }
                SystemMessage {
                    text: text.to_string(),
                    components: components_of(text, &ns),
                }
            })
            .collect();
        let sized = |t: &Value| ToolSize {
            name: t["function"]["name"]
                .as_str()
                .or_else(|| t["name"].as_str())
                .unwrap_or("?")
                .to_string(),
            chars: chars(&t.to_string()),
            schema: t.clone(),
        };
        let (tool_sizes, tools_array_chars, tools_omitted) = match tools {
            None => (vec![], 0, true),
            Some(arr) => (
                arr.iter().map(sized).collect(),
                chars(&Value::Array(arr.to_vec()).to_string()),
                false,
            ),
        };
        Self {
            system,
            tools: tool_sizes,
            tools_array_chars,
            tools_omitted,
        }
    }

    /// Convenience: extract system messages from an OpenAI-format message list.
    pub fn build(messages: &[Value], tools: Option<&[Value]>, notes: &[Annotation]) -> Self {
        let texts: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect();
        Self::build_from_parts(&texts, tools, notes)
    }

    pub fn system_chars(&self) -> usize {
        self.system.iter().map(|m| chars(&m.text)).sum()
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::from("# Request prompt report\n\nSizes are characters (the backend reports tokens only after a request).\n");
        for (i, m) in self.system.iter().enumerate() {
            out.push_str(&format!(
                "\n## System message {} ({} chars)\n\n| component | source | chars |\n|---|---|---:|\n",
                i + 1,
                chars(&m.text)
            ));
            for c in &m.components {
                out.push_str(&format!("| {} | {} | {} |\n", c.label, c.source, c.chars));
            }
        }
        if self.tools_omitted {
            out.push_str("\n## Tools (omitted by provider — no tools field sent)\n\n");
        } else {
            out.push_str(&format!(
                "\n## Tools ({} tools, {} chars as sent)\n\n| tool | chars |\n|---|---:|\n",
                self.tools.len(),
                self.tools_array_chars
            ));
        }
        if !self.tools_omitted {
            for t in &self.tools {
                out.push_str(&format!("| {} | {} |\n", t.name, t.chars));
            }
        }
        out.push_str(&format!(
            "\nTotal: system {} + tools {} = {} chars\n",
            self.system_chars(),
            self.tools_array_chars,
            self.system_chars() + self.tools_array_chars
        ));
        for (i, m) in self.system.iter().enumerate() {
            out.push_str(&format!(
                "\n## System message {} text\n\n~~~~text\n{}\n~~~~\n",
                i + 1,
                m.text
            ));
        }
        // Full schemas as dispatched — the central inspection requirement.
        if !self.tools.is_empty() {
            out.push_str("\n## Tool schemas (as sent)\n");
            for t in &self.tools {
                out.push_str(&format!(
                    "\n### {}\n\n```json\n{}\n```\n",
                    t.name,
                    serde_json::to_string_pretty(&t.schema).unwrap_or_default()
                ));
            }
        }
        out
    }
}

/// Raised by [`ReportSink::observe_request`] after a successful dump write
/// when the caller asked to stop before sending.
#[derive(Debug)]
pub struct RequestDumped(pub PathBuf);

impl std::fmt::Display for RequestDumped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request report written to {}", self.0.display())
    }
}
impl std::error::Error for RequestDumped {}

/// Session-owned capture state. One per interactive/headless session;
/// child/auxiliary calls get `None` or their own independent sink so sessions
/// cannot cross-contaminate each other's reports, dumps, or show-next flags.
pub struct ReportSink {
    state: Mutex<SinkState>,
}

impl std::fmt::Debug for ReportSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("ReportSink")
            .field("show_next", &s.show_next)
            .field("has_last", &s.last.is_some())
            .field("dump_path", &s.dump.as_ref().map(|(p, _)| p))
            .finish()
    }
}

#[derive(Default)]
struct SinkState {
    notes: Vec<Annotation>,
    last: Option<RequestReport>,
    show_next: bool,
    dump: Option<(PathBuf, bool)>,
}

impl Default for ReportSink {
    fn default() -> Self {
        Self {
            state: Mutex::new(SinkState::default()),
        }
    }
}

impl ReportSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a sink that writes the first request's report to `path` and
    /// stops before sending (prepare-only / `--dump-request` mode).
    pub fn prepare_and_dump(path: PathBuf) -> Self {
        let state = SinkState {
            dump: Some((path, true)),
            ..Default::default()
        };
        Self {
            state: Mutex::new(state),
        }
    }

    fn s(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Name a piece of the system message being assembled.
    /// A later piece with the same label replaces the earlier one.
    pub fn annotate(&self, label: &str, source: &str, text: &str) {
        let mut s = self.s();
        s.notes.retain(|n| n.label != label);
        s.notes.push(Annotation {
            label: label.into(),
            source: source.into(),
            text: text.into(),
        });
    }

    /// Arm: print the report just before the next request is sent.
    pub fn arm_show_next(&self) {
        self.s().show_next = true;
    }

    /// Consume the show-next flag; returns true if it was set.
    pub fn take_show_flag(&self) -> bool {
        std::mem::take(&mut self.s().show_next)
    }

    /// The report for the most recent request, if one has been built.
    pub fn last_report(&self) -> Option<RequestReport> {
        self.s().last.clone()
    }

    /// Snapshot of current annotations — lets the TUI command handler build a
    /// fresh report without submitting a turn.
    pub fn snapshot_notes(&self) -> Vec<Annotation> {
        self.s().notes.clone()
    }

    /// Called at the final body-construction boundary of each provider, every
    /// round and summary send. Builds and stores the report from the exact
    /// provider-format values dispatched. `tools = None` means the provider
    /// omitted the tools field (e.g. cap-exit summary); `tools = Some(arr)`
    /// means an explicit array (possibly empty). Returns `Err(RequestDumped)`
    /// when dump-and-stop mode is armed and the write succeeded; propagates
    /// write errors as-is (a failed write is never silently reported as
    /// success).
    pub fn observe_request(
        &self,
        system_texts: &[&str],
        tools: Option<&[Value]>,
    ) -> anyhow::Result<()> {
        let notes = self.s().notes.clone();
        let report = RequestReport::build_from_parts(system_texts, tools, &notes);
        let mut s = self.s();
        let dump = s.dump.take();
        if let Some((path, stop)) = dump {
            std::fs::write(&path, report.to_markdown())?;
            s.last = Some(report);
            if stop {
                return Err(RequestDumped(path).into());
            }
        } else {
            s.last = Some(report);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn note(label: &str, text: &str) -> Annotation {
        Annotation {
            label: label.into(),
            source: format!("src::{label}"),
            text: text.into(),
        }
    }

    /// The component sizes of every system message sum to the exact length of
    /// the text sent, whether pieces are annotated, overlapping, missing from
    /// the text, or absent altogether; and the tool sizes plus the array's
    /// framing equal the serialized array.
    #[test]
    fn component_sizes_sum_to_what_is_sent() {
        let card = format!("{}\nid: 1", super::super::prompt_read::ACTIVE_PROMPT_PREFIX);
        let messages = vec![
            json!({"role": "system", "content": "héllo identity\n\nWorkspace: /w\n\nrules"}),
            json!({"role": "system", "content": card}),
            json!({"role": "user", "content": "do it"}),
        ];
        let tools = vec![
            json!({"type": "function", "function": {"name": "a", "parameters": {}}}),
            json!({"type": "function", "function": {"name": "bb", "parameters": {"x": 1}}}),
        ];
        let notes = [
            note("identity", "héllo identity"),
            note("workspace", "Workspace: /w"),
            note("inner", "Workspace"), // overlaps "workspace": dropped
            note("ghost", "not in the text"),
        ];
        let r = RequestReport::build(&messages, Some(&tools), &notes);
        assert_eq!(r.system.len(), 2);
        for m in &r.system {
            let sum: usize = m.components.iter().map(|c| c.chars).sum();
            assert_eq!(sum, m.text.chars().count(), "{:?}", m.components);
        }
        let labels: Vec<&str> = r.system[0]
            .components
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["identity", UNATTRIBUTED, "workspace", UNATTRIBUTED]
        );
        assert_eq!(r.system[1].components[0].label, "active-prompt card");
        let tool_sum: usize = r.tools.iter().map(|t| t.chars).sum();
        assert_eq!(r.tools_array_chars, tool_sum + r.tools.len() + 1);
        assert_eq!(r.tools[1].name, "bb");
        assert!(r.to_markdown().contains("| bb |"));
        // #2937: full schema must appear in the markdown output
        assert!(r.to_markdown().contains("\"name\": \"bb\""));
    }

    #[test]
    fn empty_request_reports_zero() {
        // Some(&[]) = explicit empty array sent => 2 chars for `[]`
        let r = RequestReport::build(&[], Some(&[]), &[]);
        assert_eq!((r.system_chars(), r.tools_array_chars), (0, 2));
        assert!(!r.tools_omitted);
    }

    /// Two independent ReportSink instances cannot cross-contaminate: separate
    /// last reports, separate show-next flags, separate dump arms.
    #[test]
    fn session_isolation() {
        let sink_a = ReportSink::new();
        let sink_b = ReportSink::new();

        // Annotate on A does not affect B.
        sink_a.annotate("key", "src", "val");
        assert!(sink_b.s().notes.is_empty());

        // arm_show_next is independent.
        sink_a.arm_show_next();
        assert!(!sink_b.take_show_flag());
        assert!(sink_a.take_show_flag());

        // observe on A populates A's last but not B's.
        sink_a
            .observe_request(&["hello"], Some(&[json!({"name": "t1"})]))
            .unwrap();
        assert!(sink_a.last_report().is_some());
        assert!(sink_b.last_report().is_none());
    }

    /// observe_request includes the full schema JSON in to_markdown().
    #[test]
    fn schema_in_markdown() {
        let sink = ReportSink::new();
        let tool = json!({"type": "function", "function": {"name": "my_tool", "description": "does stuff", "parameters": {"type": "object", "properties": {"arg": {"type": "string"}}}}});
        sink.observe_request(&["sys text"], Some(&[tool])).unwrap();
        let md = sink.last_report().unwrap().to_markdown();
        // Should contain the description from the schema
        assert!(
            md.contains("does stuff"),
            "schema body missing from markdown"
        );
        assert!(md.contains("my_tool"), "tool name missing");
    }

    /// A failed dump write propagates as an error; last is not set to a stale
    /// value, and the sink remains usable.
    #[test]
    fn dump_failure_propagates() {
        // Use a directory path as the dump target — fs::write to a dir fails.
        let dir = std::env::temp_dir();
        let sink = ReportSink::prepare_and_dump(dir.clone());
        let err = sink
            .observe_request(&["sys"], Some(&[]))
            .expect_err("should fail writing to a directory");
        // The error is NOT a RequestDumped (write failed before we could stop).
        assert!(
            err.downcast_ref::<RequestDumped>().is_none(),
            "write failure should not look like a successful dump"
        );
        // last_report is None — we never stored a stale report.
        assert!(sink.last_report().is_none());
    }

    /// None tools (omitted) vs Some(&[]) (empty array) are distinguishable.
    #[test]
    fn tools_omitted_vs_empty() {
        let r_omitted = RequestReport::build_from_parts(&["sys"], None, &[]);
        assert!(r_omitted.tools_omitted);
        assert_eq!(r_omitted.tools_array_chars, 0);
        assert!(r_omitted.to_markdown().contains("omitted by provider"));

        let r_empty = RequestReport::build_from_parts(&["sys"], Some(&[]), &[]);
        assert!(!r_empty.tools_omitted);
        assert_eq!(r_empty.tools_array_chars, 2);
        assert!(r_empty.to_markdown().contains("0 tools, 2 chars"));
    }

    /// arm_show_next is consumed exactly once by take_show_flag.
    #[test]
    fn show_flag_consumed_once() {
        let sink = ReportSink::new();
        sink.arm_show_next();
        sink.observe_request(&["sys"], Some(&[])).unwrap();
        assert!(
            sink.take_show_flag(),
            "flag should be set after arm_show_next"
        );
        assert!(
            !sink.take_show_flag(),
            "flag should be clear after first consumption"
        );
    }

    /// snapshot_notes returns annotations without draining them.
    #[test]
    fn snapshot_notes_independent() {
        let sink = ReportSink::new();
        sink.annotate("k1", "s1", "v1");
        sink.annotate("k2", "s2", "v2");
        let snap = sink.snapshot_notes();
        assert_eq!(snap.len(), 2);
        // Notes still in sink after snapshot.
        assert_eq!(sink.snapshot_notes().len(), 2);
    }
}
