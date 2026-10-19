//! #2937: the final request, made visible. Measurement only — nothing here
//! changes or prunes what is sent.
//!
//! Each wire loop calls [`observe_request`] with the exact `messages` and
//! `tools` it is about to send, so the report is read off the real request and
//! cannot drift from it. Assemblers may [`annotate`] the pieces they join into
//! a system message (label + source); the report locates each piece in the
//! sent text and reports every remaining byte as an explicit `(unattributed)`
//! component, so the component sizes always sum to the exact message length.
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
}

#[derive(Clone, Debug)]
pub struct RequestReport {
    pub system: Vec<SystemMessage>,
    pub tools: Vec<ToolSize>,
    /// Length of the serialized `tools` array; exceeds the sum of `tools` by
    /// the array's brackets and commas.
    pub tools_array_chars: usize,
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
    /// Measure the request as sent: every leading-or-not `system` message and
    /// the `tools` array.
    pub fn build(messages: &[Value], tools: &[Value], notes: &[Annotation]) -> Self {
        let card = Annotation {
            label: "active-prompt card".into(),
            source: "agentic::prompt_read::active_prompt_card".into(),
            text: String::new(),
        };
        let system = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .map(|text| {
                let mut notes = notes.to_vec();
                if text.starts_with(super::prompt_read::ACTIVE_PROMPT_PREFIX) {
                    notes.push(Annotation {
                        text: text.to_string(),
                        ..card.clone()
                    });
                }
                SystemMessage {
                    text: text.to_string(),
                    components: components_of(text, &notes),
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
        };
        Self {
            system,
            tools: tools.iter().map(sized).collect(),
            tools_array_chars: chars(&Value::Array(tools.to_vec()).to_string()),
        }
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
        out.push_str(&format!(
            "\n## Tools ({} tools, {} chars as sent)\n\n| tool | chars |\n|---|---:|\n",
            self.tools.len(),
            self.tools_array_chars
        ));
        for t in &self.tools {
            out.push_str(&format!("| {} | {} |\n", t.name, t.chars));
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
        out
    }
}

/// Raised by [`observe_request`] after a dump when the caller asked to stop
/// before anything is sent.
#[derive(Debug)]
pub struct RequestDumped(pub PathBuf);

impl std::fmt::Display for RequestDumped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request report written to {}", self.0.display())
    }
}
impl std::error::Error for RequestDumped {}

#[derive(Default)]
struct Probe {
    notes: Vec<Annotation>,
    last: Option<RequestReport>,
    show_next: bool,
    dump: Option<(PathBuf, bool)>,
}

static PROBE: Mutex<Probe> = Mutex::new(Probe {
    notes: Vec::new(),
    last: None,
    show_next: false,
    dump: None,
});

fn probe() -> std::sync::MutexGuard<'static, Probe> {
    PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Name a piece of the system message being assembled. A later piece with the
/// same label replaces the earlier one; pieces that are no longer in the sent
/// text simply do not match.
pub fn annotate(label: &str, source: &str, text: &str) {
    let mut p = probe();
    p.notes.retain(|n| n.label != label);
    p.notes.push(Annotation {
        label: label.into(),
        source: source.into(),
        text: text.into(),
    });
}

/// The report for the most recent request, if one has been built.
pub fn last_report() -> Option<RequestReport> {
    probe().last.clone()
}

/// Print the report just before the next request is sent.
pub fn show_next() {
    probe().show_next = true;
}

/// Write the next request's report (markdown) to `path`; with `stop`, fail the
/// request with [`RequestDumped`] instead of sending it.
pub fn dump_next_to(path: PathBuf, stop: bool) {
    probe().dump = Some((path, stop));
}

/// Called by each wire loop with the request it is about to send.
pub(crate) fn observe_request(messages: &[Value], tools: &Value) -> anyhow::Result<()> {
    let mut p = probe();
    let report = RequestReport::build(
        messages,
        tools.as_array().map_or(&[], Vec::as_slice),
        &p.notes,
    );
    p.last = Some(report.clone());
    let show = std::mem::take(&mut p.show_next);
    let dump = p.dump.take();
    drop(p);
    if show {
        println!("{}", report.to_markdown());
    }
    if let Some((path, stop)) = dump {
        std::fs::write(&path, report.to_markdown())?;
        if stop {
            return Err(RequestDumped(path).into());
        }
    }
    Ok(())
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
        let r = RequestReport::build(&messages, &tools, &notes);
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
    }

    #[test]
    fn empty_request_reports_zero() {
        let r = RequestReport::build(&[], &[], &[]);
        assert_eq!((r.system_chars(), r.tools_array_chars), (0, 2));
    }
}
