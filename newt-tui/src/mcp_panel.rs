//! Inspect MCP connections and metadata through the shared modal panel driver.
//! Actions are intents returned to the session; drawing and navigation perform no I/O.

use crate::config_panel::{hint_line, render_panel, RowView};
use crate::list_cursor::ListCursor;
use crate::mcp::{Confinement, McpStatus};
use crate::panel::{Flow, Key, Screen};
use newt_core::mcp::{McpServerEntry, McpSource, TransportKind};
use newt_mcp_client::RemoteTool;

pub(crate) struct ServerView {
    pub entry: McpServerEntry,
    pub source: McpSource,
    pub status: Option<McpStatus>,
    pub auth: String,
    pub tools: Vec<RemoteTool>,
    pub muted: bool,
    pub conflict: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionKind {
    Login,
    Reconnect,
    Test,
}

pub(crate) struct Action {
    pub server: String,
    pub kind: ActionKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Servers,
    Server,
    Tools,
    Tool,
}

pub(crate) struct McpPanel {
    servers: Vec<ServerView>,
    server: ListCursor,
    actions: ListCursor,
    tools: Vec<ListCursor>,
    page: Page,
    scroll: ListCursor,
    width: std::cell::Cell<usize>,
    sanitize_names: bool,
    action: Option<Action>,
    pub message: String,
}

fn source_label(source: McpSource) -> &'static str {
    match source {
        McpSource::Configuration => "Newt configuration",
        McpSource::UserMcpFile => "Newt MCP file",
        McpSource::ClaudeUser => "Claude user (borrowed)",
        McpSource::ClaudeProject => "Project MCP (borrowed)",
    }
}

fn clean(text: &str) -> String {
    newt_core::tty::width::strip_ansi(text)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

fn effect(tool: &RemoteTool) -> &'static str {
    match tool
        .annotations
        .as_ref()
        .and_then(|a| a.get("readOnlyHint"))
        .and_then(|v| v.as_bool())
    {
        Some(true) => "read hint",
        Some(false) => "write hint",
        None => "unknown",
    }
}

/// Show only the transport origin, never URL userinfo, path, query, or fragment.
fn endpoint(entry: &McpServerEntry) -> String {
    if entry.transport == TransportKind::Stdio {
        return entry
            .command
            .as_deref()
            .and_then(|command| std::path::Path::new(command).file_name())
            .map_or_else(
                || "stdio".into(),
                |name| format!("stdio · {}", name.to_string_lossy()),
            );
    }
    entry
        .url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| {
            url.host_str().map(|host| {
                format!(
                    "{}://{}{}",
                    url.scheme(),
                    host,
                    url.port()
                        .map_or_else(String::new, |port| format!(":{port}"))
                )
            })
        })
        .unwrap_or_else(|| "invalid endpoint".into())
}

fn connection(server: &ServerView) -> String {
    if server.conflict.is_some() {
        return "namespace conflict".into();
    }
    match &server.status {
        Some(McpStatus::Connected { tools, .. }) => format!(
            "connected · {tools} tools{}",
            if server.muted { " · muted" } else { "" }
        ),
        Some(McpStatus::Disabled) => "disabled".into(),
        Some(McpStatus::Skipped(_)) => "disconnected · reconnect to diagnose".into(),
        None => "not connected".into(),
    }
}

impl McpPanel {
    pub(crate) fn new(mut servers: Vec<ServerView>, sanitize_names: bool) -> Self {
        servers.sort_by_key(|server| server.source);
        let tools = servers
            .iter()
            .map(|server| ListCursor::new(server.tools.len(), 16, 0))
            .collect();
        Self {
            server: ListCursor::new(servers.len(), 16, 0),
            servers,
            tools,
            actions: ListCursor::new(4, 16, 0),
            page: Page::Servers,
            scroll: ListCursor::new(0, 16, 0),
            width: std::cell::Cell::new(76),
            sanitize_names,
            action: None,
            message: String::new(),
        }
    }

    pub(crate) fn take_action(&mut self) -> Option<Action> {
        self.action.take()
    }

    pub(crate) fn refresh(&mut self, servers: Vec<ServerView>) {
        let selected = self
            .servers
            .get(self.server.at())
            .map(|server| (server.entry.name.clone(), server.source));
        let old_tools = self
            .tools
            .iter()
            .map(|cursor| cursor.at())
            .collect::<Vec<_>>();
        self.servers = servers;
        self.servers.sort_by_key(|server| server.source);
        let at = selected
            .and_then(|(name, source)| {
                self.servers
                    .iter()
                    .position(|server| server.entry.name == name && server.source == source)
            })
            .unwrap_or(0);
        self.server = ListCursor::new(self.servers.len(), 16, at);
        self.tools = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, server)| {
                ListCursor::new(
                    server.tools.len(),
                    16,
                    old_tools.get(i).copied().unwrap_or(0),
                )
            })
            .collect();
    }

    fn request(&mut self, kind: ActionKind) -> Flow {
        let Some(server) = self.servers.get(self.server.at()) else {
            return Flow::Stay;
        };
        if server.conflict.is_some() {
            return Flow::Stay;
        }
        self.action = Some(Action {
            server: server.entry.name.clone(),
            kind,
        });
        Flow::Close(true)
    }

    fn tool_lines(&self) -> Vec<String> {
        let Some(server) = self.servers.get(self.server.at()) else {
            return vec![];
        };
        let Some(tool) = server.tools.get(self.tools[self.server.at()].at()) else {
            return vec!["No advertised tools".into()];
        };
        let prefix = newt_core::mcp::runtime_server_prefix(&server.entry.name, self.sanitize_names);
        let mut lines = vec![
            format!("Name: {}", tool.name),
            format!("Full name: {prefix}__{}", tool.name),
            format!("Effect: {} (server metadata, not permission)", effect(tool)),
            format!("Description: {}", tool.description),
            "Parameters:".into(),
        ];
        let required = tool.input_schema.get("required").and_then(|v| v.as_array());
        if let Some(properties) = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
        {
            for (name, schema) in properties {
                let ty = schema.get("type").map_or_else(
                    || "unspecified".into(),
                    |v| v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                );
                let required =
                    required.is_some_and(|fields| fields.iter().any(|v| v.as_str() == Some(name)));
                lines.push(format!(
                    "{name}: {ty} · {}",
                    if required { "required" } else { "optional" }
                ));
                if let Some(description) = schema.get("description").and_then(|v| v.as_str()) {
                    lines.push(description.into());
                }
            }
        }
        lines.push(format!("Full schema: {}", tool.input_schema));
        lines
            .into_iter()
            .flat_map(|line| {
                clean(&line)
                    .lines()
                    .flat_map(|line| newt_core::tty::wrap_line(line, self.width.get()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn rows(&self) -> Vec<RowView> {
        let row = |value: String, selected| RowView {
            label: "",
            value: clean(&value),
            provenance: String::new(),
            selected,
            editable: false,
        };
        let Some(server) = self.servers.get(self.server.at()) else {
            return vec![row("No MCP servers discovered".into(), false)];
        };
        match self.page {
            Page::Servers => {
                let mut rows = Vec::new();
                let mut source = None;
                for (i, server) in self.servers.iter().enumerate() {
                    if source != Some(server.source) {
                        rows.push(row(source_label(server.source).into(), false));
                        source = Some(server.source);
                    }
                    rows.push(row(
                        format!("{} · {}", server.entry.name, connection(server)),
                        i == self.server.at(),
                    ));
                }
                rows
            }
            Page::Server => {
                let ocap = match &server.status {
                    Some(McpStatus::Connected {
                        confinement, net, ..
                    }) => format!(
                        "{}{}",
                        match confinement {
                            Confinement::Confined(kind) => format!("confined ({kind})"),
                            Confinement::Advisory => "advisory".into(),
                            Confinement::Remote => "remote server".into(),
                        },
                        net.note()
                    ),
                    _ => {
                        if newt_core::mcp::admit(&server.entry).is_err() {
                            "configuration not admitted".into()
                        } else {
                            "not established; reconnect checks live grants".into()
                        }
                    }
                };
                let mut rows = vec![
                    row(format!("Server: {}", server.entry.name), false),
                    row(format!("Source: {}", source_label(server.source)), false),
                    row(format!("Connection: {}", connection(server)), false),
                    row(format!("Authentication: {}", server.auth), false),
                    row(format!("OCAP: {ocap}"), false),
                    row(format!("Endpoint: {}", endpoint(&server.entry)), false),
                ];
                if let Some(winner) = &server.conflict {
                    rows.push(row(
                        format!(
                            "Namespace conflict: {winner} wins; edit configuration to resolve."
                        ),
                        false,
                    ));
                } else {
                    for (i, label) in [
                        "Tools — inspect metadata",
                        "Login",
                        "Reconnect — fresh connection",
                        "Test — list tools only",
                    ]
                    .iter()
                    .enumerate()
                    {
                        rows.push(row((*label).into(), self.actions.at() == i));
                    }
                }
                rows
            }
            Page::Tools => {
                let mut rows = vec![row(
                    format!(
                        "{} · effect hints are server metadata, not permission",
                        server.entry.name
                    ),
                    false,
                )];
                if server.tools.is_empty() {
                    rows.push(row(
                        "No advertised tools; reconnect or test the server.".into(),
                        false,
                    ));
                }
                rows.extend(server.tools.iter().enumerate().map(|(i, tool)| {
                    row(
                        format!("{} · {}", tool.name, effect(tool)),
                        i == self.tools[self.server.at()].at(),
                    )
                }));
                rows
            }
            Page::Tool => self
                .tool_lines()
                .into_iter()
                .enumerate()
                .map(|(i, line)| row(line, i == self.scroll.at()))
                .collect(),
        }
    }
}

impl Screen for McpPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        self.width
            .set(usize::from(frame.area().width.saturating_sub(7)).max(1));
        render_panel(
            frame,
            match self.page {
                Page::Servers => "MCP servers",
                Page::Server => "MCP server",
                Page::Tools => "MCP tools",
                Page::Tool => "MCP tool details",
            },
            &self.rows(),
            if self.page == Page::Server && !self.message.is_empty() {
                crate::config_panel::status_line(&format!("Esc back · {}", clean(&self.message)))
            } else {
                hint_line("↑↓ navigate · Enter select · Esc back")
            },
            0,
            0,
        );
    }
    fn key(&mut self, key: Key) -> Flow {
        if matches!(key, Key::Esc | Key::Ctrl('c')) {
            self.page = match self.page {
                Page::Tool => Page::Tools,
                Page::Tools => Page::Server,
                Page::Server => Page::Servers,
                Page::Servers => return Flow::Close(false),
            };
            return Flow::Stay;
        }
        if self.servers.is_empty() {
            return Flow::Stay;
        }
        if key == Key::Enter {
            self.page = match self.page {
                Page::Servers => {
                    self.message.clear();
                    Page::Server
                }
                Page::Server => {
                    if self.servers[self.server.at()].conflict.is_some() {
                        return Flow::Stay;
                    }
                    match self.actions.at() {
                        0 => Page::Tools,
                        1 => return self.request(ActionKind::Login),
                        2 => return self.request(ActionKind::Reconnect),
                        _ => return self.request(ActionKind::Test),
                    }
                }
                Page::Tools => {
                    self.scroll = ListCursor::new(self.tool_lines().len(), 16, 0);
                    Page::Tool
                }
                Page::Tool => Page::Tool,
            };
        }
        let delta = match key {
            Key::Up | Key::Char('k') => -1,
            Key::Down | Key::Char('j') => 1,
            Key::Ctrl('u') => -15,
            Key::Ctrl('d') => 15,
            _ => 0,
        };
        match self.page {
            Page::Servers => self.server.step(delta),
            Page::Server => self.actions.step(delta),
            Page::Tools => self.tools[self.server.at()].step(delta),
            Page::Tool => {
                self.scroll = ListCursor::new(self.tool_lines().len(), 16, self.scroll.at());
                self.scroll.step(delta);
            }
        }
        Flow::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{Confinement, McpStatus, NetGate};
    use crate::panel::{Flow, Key, Screen};
    use newt_core::mcp::{McpServerEntry, McpSource};
    use newt_mcp_client::RemoteTool;
    use serde_json::json;

    fn server(name: &str) -> ServerView {
        let entry: McpServerEntry = toml::from_str(&format!(
            "name = '{name}'\ntype = 'http'\nurl = 'https://private-user:private-pass@example.test/mcp?access_token=private-token#private-fragment'\n"
        )).unwrap();
        ServerView {
            entry,
            source: McpSource::Configuration,
            status: Some(McpStatus::Connected {
                tools: 2,
                confinement: Confinement::Remote,
                net: NetGate::Gated(1),
            }),
            auth: "saved login valid".into(),
            tools: vec![
                RemoteTool {
                    name: "lookup".into(),
                    description: "Find a document".into(),
                    input_schema: json!({"type":"object", "properties":{"query":{"type":"string"},"limit":{"type":"integer"}}, "required":["query"]}),
                    meta: None,
                    annotations: Some(json!({"readOnlyHint":true})),
                },
                RemoteTool {
                    name: "unknown_operation".into(),
                    description: "No effect metadata provided".into(),
                    input_schema: json!({"type":"object"}),
                    meta: None,
                    annotations: None,
                },
            ],
            muted: false,
            conflict: None,
        }
    }

    fn render(panel: &McpPanel, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| panel.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn management_navigation_preserves_selection_and_never_executes_inspection() {
        let mut panel = McpPanel::new(vec![server("first"), server("second")], true);
        assert_eq!(panel.key(Key::Down), Flow::Stay);
        panel.key(Key::Enter);
        assert!(render(&panel, 90, 25).contains("second"));
        panel.key(Key::Enter); // Tools is the first detail row.
        panel.key(Key::Down);
        panel.key(Key::Enter);
        let text = render(&panel, 90, 25);
        assert!(text.contains("second__unknown_operation"), "{text}");
        assert!(
            text.contains("unknown") && text.contains("metadata"),
            "{text}"
        );
        assert!(panel.take_action().is_none());
        assert_eq!(panel.key(Key::Esc), Flow::Stay); // tool → tools
        panel.key(Key::Enter);
        assert!(render(&panel, 90, 25).contains("second__unknown_operation"));
        panel.key(Key::Esc);
        assert_eq!(panel.key(Key::Esc), Flow::Stay); // tools → server
        assert_eq!(panel.key(Key::Esc), Flow::Stay); // server → list
        panel.key(Key::Enter);
        assert!(render(&panel, 90, 25).contains("second"));
        panel.key(Key::Esc);
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));
        assert!(panel.take_action().is_none());
    }

    #[test]
    fn management_details_redact_endpoints_and_explain_tool_parameters() {
        let mut panel = McpPanel::new(vec![server("documents")], true);
        panel.key(Key::Enter);
        let text = render(&panel, 100, 27);
        assert!(text.contains("example.test"), "{text}");
        for secret in [
            "private-user",
            "private-pass",
            "private-token",
            "private-fragment",
        ] {
            assert!(
                !text.contains(secret),
                "endpoint credentials escaped redaction"
            );
        }
        assert!(
            text.contains("Connection") && text.contains("Authentication") && text.contains("OCAP"),
            "{text}"
        );
        panel.key(Key::Enter);
        panel.key(Key::Enter);
        let text = render(&panel, 100, 27);
        for expected in [
            "documents__lookup",
            "Find a document",
            "query",
            "required",
            "string",
            "limit",
            "integer",
            "metadata",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(panel.take_action().is_none());
    }

    #[test]
    fn management_conflicts_have_no_login_reconnect_or_test_actions() {
        let mut shadowed = server("shadowed");
        shadowed.source = McpSource::ClaudeProject;
        shadowed.conflict = Some("higher-precedence server".into());
        let mut panel = McpPanel::new(vec![shadowed], true);
        panel.key(Key::Enter);
        let text = render(&panel, 100, 27);
        assert!(
            text.contains("conflict") && text.contains("higher-precedence server"),
            "{text}"
        );
        for key in [Key::Char('l'), Key::Char('r'), Key::Char('t'), Key::Enter] {
            panel.key(key);
            assert!(panel.take_action().is_none());
        }
    }

    #[test]
    fn management_action_result_stays_visible_in_a_small_terminal() {
        let mut panel = McpPanel::new(vec![server("documents")], true);
        panel.key(Key::Enter);
        panel.message = "Reconnect failed".into();
        let text = render(&panel, 40, 12);
        assert!(
            text.contains("Reconnect failed") && text.contains("Esc"),
            "{text}"
        );
    }

    #[test]
    fn management_panel_fits_small_terminal_and_keeps_back_hint() {
        let panel = McpPanel::new(vec![server("documents")], true);
        for (width, height) in [(80, 17), (40, 12)] {
            let text = render(&panel, width, height);
            assert!(
                text.contains("MCP") && text.contains("documents") && text.contains("Esc"),
                "{text}"
            );
        }
    }
}
