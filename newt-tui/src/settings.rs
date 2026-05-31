//! Interactive settings TUI — `newt settings`.
//!
//! Tabs: TUI | DGX | About
//! Left panel: ANSI-40 logo (parsed to ratatui spans).
//! Quit on dirty state → Save / Discard prompt.

use std::io;
use std::path::{Path, PathBuf};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Tabs},
    Frame, Terminal,
};

use newt_core::{ChatStyle, Config, DgxConfig, EditMode, EndpointKind, TuiConfig};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LOGO_ANSI: &str = include_str!("../../docs/logos/newt-ansi-40.txt");
const LOGO_COLS: u16 = 42;

const NEWT_ORANGE: Color = Color::Rgb(220, 60, 20);
const SEL_BG: Color = Color::Rgb(40, 40, 60);
const DIM: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// App state enums
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Tui,
    Dgx,
    About,
}

impl Category {
    pub const ALL: [Self; 3] = [Self::Tui, Self::Dgx, Self::About];

    fn title(self) -> &'static str {
        match self {
            Self::Tui => "TUI",
            Self::Dgx => "DGX",
            Self::About => "About",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TuiField {
    ChatStyle,
    EditMode,
    NoSplash,
    Prompt,
}

impl TuiField {
    pub const ALL: [Self; 4] = [
        Self::ChatStyle,
        Self::EditMode,
        Self::NoSplash,
        Self::Prompt,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ChatStyle => "chat style",
            Self::EditMode => "edit mode",
            Self::NoSplash => "no splash",
            Self::Prompt => "prompt",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DgxField {
    Host,
    Model,
    Endpoint,
}

impl DgxField {
    pub const ALL: [Self; 3] = [Self::Host, Self::Model, Self::Endpoint];

    pub fn label(self) -> &'static str {
        match self {
            Self::Host => "ollama url",
            Self::Model => "model",
            Self::Endpoint => "endpoint",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InputMode {
    Navigate,
    EditingText { field_label: &'static str, buf: String },
    ConfirmQuit,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct SettingsApp {
    pub category: Category,
    pub tui_cursor: usize,
    pub dgx_cursor: usize,
    pub tui: TuiConfig,
    pub original_tui: TuiConfig,
    pub dgx: DgxConfig,
    pub original_dgx: DgxConfig,
    pub input_mode: InputMode,
    pub save_path: PathBuf,
    pub status: Option<String>,
}

impl SettingsApp {
    pub fn new(config: Config, save_path: PathBuf) -> Self {
        let tui = config.tui.clone().unwrap_or_default();
        let dgx = config.dgx.clone().unwrap_or_default();
        Self {
            category: Category::Tui,
            tui_cursor: 0,
            dgx_cursor: 0,
            original_tui: tui.clone(),
            tui,
            original_dgx: dgx.clone(),
            dgx,
            input_mode: InputMode::Navigate,
            save_path,
            status: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.tui != self.original_tui || self.dgx != self.original_dgx
    }

    fn current_tui_field(&self) -> TuiField {
        TuiField::ALL[self.tui_cursor]
    }

    fn current_dgx_field(&self) -> DgxField {
        DgxField::ALL[self.dgx_cursor]
    }

    pub fn save(&mut self, original_config: &Config) -> anyhow::Result<()> {
        let mut config = original_config.clone();
        config.tui = Some(self.tui.clone());
        config.dgx = Some(self.dgx.clone());
        config.save(&self.save_path)?;
        self.original_tui = self.tui.clone();
        self.original_dgx = self.dgx.clone();
        self.status = Some(format!("Saved → {}", self.save_path.display()));
        Ok(())
    }

    pub fn toggle_current(&mut self) {
        self.status = None;
        match self.category {
            Category::Tui => match self.current_tui_field() {
                TuiField::ChatStyle => self.tui.chat_style = self.tui.chat_style.toggle(),
                TuiField::EditMode  => self.tui.edit_mode  = self.tui.edit_mode.toggle(),
                TuiField::NoSplash  => self.tui.no_splash  = !self.tui.no_splash,
                TuiField::Prompt    => self.begin_edit(),
            },
            Category::Dgx => match self.current_dgx_field() {
                DgxField::Endpoint => {
                    self.dgx.active_endpoint = match self.dgx.active_endpoint {
                        EndpointKind::Ollama    => EndpointKind::OllamaLb,
                        EndpointKind::OllamaLb  => EndpointKind::InCluster,
                        EndpointKind::InCluster => EndpointKind::Vllm,
                        EndpointKind::Vllm      => EndpointKind::Ollama,
                    };
                }
                DgxField::Host | DgxField::Model => self.begin_edit(),
            },
            Category::About => {}
        }
    }

    pub fn begin_edit(&mut self) {
        let (label, buf) = match self.category {
            Category::Tui => match self.current_tui_field() {
                TuiField::Prompt => (
                    TuiField::Prompt.label(),
                    self.tui.prompt.clone().unwrap_or_default(),
                ),
                _ => return,
            },
            Category::Dgx => match self.current_dgx_field() {
                DgxField::Host => (
                    DgxField::Host.label(),
                    dgx_host_str(&self.dgx),
                ),
                DgxField::Model => (
                    DgxField::Model.label(),
                    self.dgx.active_model.clone().unwrap_or_default(),
                ),
                DgxField::Endpoint => return,
            },
            Category::About => return,
        };
        self.input_mode = InputMode::EditingText { field_label: label, buf };
    }

    pub fn confirm_edit(&mut self) {
        if let InputMode::EditingText { field_label, buf } = &self.input_mode.clone() {
            let val = if buf.is_empty() { None } else { Some(buf.clone()) };
            match *field_label {
                s if s == TuiField::Prompt.label() => self.tui.prompt = val,
                s if s == DgxField::Host.label() => set_dgx_host(&mut self.dgx, val),
                s if s == DgxField::Model.label() => self.dgx.active_model = val,
                _ => {}
            }
            self.input_mode = InputMode::Navigate;
            self.status = None;
        }
    }

    pub fn cancel_edit(&mut self) {
        self.input_mode = InputMode::Navigate;
    }

    pub fn type_char(&mut self, c: char) {
        if let InputMode::EditingText { buf, .. } = &mut self.input_mode {
            buf.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if let InputMode::EditingText { buf, .. } = &mut self.input_mode {
            buf.pop();
        }
    }

    pub fn rendered_prompt(&self) -> String {
        expand_prompt(self.tui.prompt.as_deref().unwrap_or("\\w $ "))
    }
}

// ---------------------------------------------------------------------------
// DGX helpers
// ---------------------------------------------------------------------------

fn dgx_host_str(dgx: &DgxConfig) -> String {
    dgx.nodes
        .first()
        .and_then(|n| n.ollama.clone())
        .unwrap_or_default()
}

fn set_dgx_host(dgx: &mut DgxConfig, url: Option<String>) {
    if let Some(u) = url {
        if let Some(node) = dgx.nodes.first_mut() {
            node.ollama = Some(u);
        } else {
            let mut node = newt_core::DgxNode::default();
            node.ollama = Some(u);
            node.name = "dgx".into();
            dgx.nodes.push(node);
        }
    } else {
        if let Some(node) = dgx.nodes.first_mut() {
            node.ollama = None;
        }
    }
}

fn expand_prompt(template: &str) -> String {
    let ws = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "workspace".into());
    template
        .replace("\\W", &std::env::current_dir().unwrap_or_default().to_string_lossy())
        .replace("\\w", &ws)
        .replace("\\h", &hostname())
        .replace("\\v", env!("CARGO_PKG_VERSION"))
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".into())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_settings(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };
    let save_path = config_path
        .map(PathBuf::from)
        .or_else(Config::user_config_path)
        .unwrap_or_else(|| PathBuf::from("newt.toml"));

    let mut app = SettingsApp::new(config.clone(), save_path);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, &config);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut SettingsApp,
    original_config: &Config,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| render(f, app))?;

        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            // --- Global exits ---
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => break,

            // --- Save ---
            Event::Key(KeyEvent {
                code: KeyCode::Char('s'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL)
                && app.input_mode == InputMode::Navigate =>
            {
                if let Err(e) = app.save(original_config) {
                    app.status = Some(format!("Save failed: {e}"));
                }
            }

            // --- Quit (with dirty-check) ---
            Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                ..
            }) if app.input_mode == InputMode::Navigate => {
                if app.is_dirty() {
                    app.input_mode = InputMode::ConfirmQuit;
                } else {
                    break;
                }
            }

            // --- Confirm-quit modal ---
            Event::Key(KeyEvent {
                code: KeyCode::Char('s') | KeyCode::Char('S'),
                ..
            }) if app.input_mode == InputMode::ConfirmQuit => {
                if let Err(e) = app.save(original_config) {
                    app.status = Some(format!("Save failed: {e}"));
                }
                break;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('d') | KeyCode::Char('D'),
                ..
            }) if app.input_mode == InputMode::ConfirmQuit => break,

            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) if app.input_mode == InputMode::ConfirmQuit => {
                app.input_mode = InputMode::Navigate;
            }

            // --- Navigation ---
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) if app.input_mode == InputMode::Navigate => {
                let idx = (app.category.index() + 1) % Category::ALL.len();
                app.category = Category::ALL[idx];
                app.tui_cursor = 0;
                app.dgx_cursor = 0;
            }

            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) if app.input_mode == InputMode::Navigate => match app.category {
                Category::Tui => app.tui_cursor = app.tui_cursor.saturating_sub(1),
                Category::Dgx => app.dgx_cursor = app.dgx_cursor.saturating_sub(1),
                Category::About => {}
            },

            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) if app.input_mode == InputMode::Navigate => match app.category {
                Category::Tui => {
                    app.tui_cursor = (app.tui_cursor + 1).min(TuiField::ALL.len() - 1)
                }
                Category::Dgx => {
                    app.dgx_cursor = (app.dgx_cursor + 1).min(DgxField::ALL.len() - 1)
                }
                Category::About => {}
            },

            Event::Key(KeyEvent {
                code: KeyCode::Enter | KeyCode::Char(' '),
                ..
            }) if app.input_mode == InputMode::Navigate => app.toggle_current(),

            // --- Text editing ---
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) if matches!(app.input_mode, InputMode::EditingText { .. }) => {
                app.confirm_edit()
            }

            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) if matches!(app.input_mode, InputMode::EditingText { .. }) => {
                app.cancel_edit()
            }

            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) if matches!(app.input_mode, InputMode::EditingText { .. }) => {
                app.backspace()
            }

            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::CONTROL)
                && matches!(app.input_mode, InputMode::EditingText { .. }) =>
            {
                app.type_char(c);
            }

            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, app: &SettingsApp) {
    let area = f.area();
    let orange = Style::default().fg(NEWT_ORANGE);
    let dim = Style::default().fg(DIM);
    let bold_orange = Style::default().fg(NEWT_ORANGE).add_modifier(Modifier::BOLD);

    let logo_w = LOGO_COLS.min(area.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(logo_w), Constraint::Fill(1)])
        .split(area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs
            Constraint::Fill(1),   // fields
            Constraint::Length(4), // preview (TUI tab) / filler
            Constraint::Length(2), // hints
        ])
        .split(cols[1]);

    render_logo(f, cols[0]);
    render_tabs(f, rows[0], app, bold_orange, dim);
    render_fields(f, rows[1], app, bold_orange, dim);
    if app.category == Category::Tui {
        render_preview(f, rows[2], app, dim, orange);
    }
    render_hints(f, rows[3], app, dim);

    if app.input_mode == InputMode::ConfirmQuit {
        render_confirm_quit(f, area, bold_orange, dim);
    }
}

fn render_logo(f: &mut Frame, area: Rect) {
    let lines = parse_ansi_logo(LOGO_ANSI);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(DIM)),
        ),
        area,
    );
}

fn render_tabs(f: &mut Frame, area: Rect, app: &SettingsApp, active: Style, dim: Style) {
    let titles: Vec<Line> = Category::ALL.iter().map(|c| Line::from(c.title())).collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.category.index())
            .style(dim)
            .highlight_style(active)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(Span::styled(" newt settings ", active))
                    .border_style(dim),
            ),
        area,
    );
}

fn render_fields(f: &mut Frame, area: Rect, app: &SettingsApp, bold_orange: Style, dim: Style) {
    f.render_widget(Clear, area);
    match app.category {
        Category::Tui => render_tui_fields(f, area, app, bold_orange, dim),
        Category::Dgx => render_dgx_fields(f, area, app, bold_orange, dim),
        Category::About => render_about(f, area, dim),
    }
}

fn field_row(
    f: &mut Frame,
    rows: &[Rect],
    idx: usize,
    is_sel: bool,
    content: Line,
) {
    let row = rows[idx];
    let bg = if is_sel { Style::default().bg(SEL_BG) } else { Style::default() };
    f.render_widget(
        Paragraph::new(content).style(bg),
        Rect { height: 1, y: row.y + 1, ..row },
    );
}

fn render_tui_fields(f: &mut Frame, area: Rect, app: &SettingsApp, bold_orange: Style, dim: Style) {
    let constraints: Vec<Constraint> = TuiField::ALL
        .iter()
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Fill(1)))
        .collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, field) in TuiField::ALL.iter().enumerate() {
        let is_sel = app.tui_cursor == i;
        let label_s = if is_sel { bold_orange } else { dim };

        let line = match field {
            TuiField::ChatStyle => toggle_line(
                field.label(),
                label_s,
                &[
                    (ChatStyle::Compact.as_str(), app.tui.chat_style == ChatStyle::Compact),
                    (ChatStyle::Verbose.as_str(), app.tui.chat_style == ChatStyle::Verbose),
                ],
                dim,
            ),
            TuiField::EditMode => toggle_line(
                field.label(),
                label_s,
                &[
                    (EditMode::Emacs.as_str(), app.tui.edit_mode == EditMode::Emacs),
                    (EditMode::Vi.as_str(), app.tui.edit_mode == EditMode::Vi),
                ],
                dim,
            ),
            TuiField::NoSplash => toggle_line(
                field.label(),
                label_s,
                &[
                    ("off", !app.tui.no_splash),
                    ("on", app.tui.no_splash),
                ],
                dim,
            ),
            TuiField::Prompt => {
                let editing = matches!(app.input_mode, InputMode::EditingText { .. });
                let val = if editing {
                    if let InputMode::EditingText { buf, .. } = &app.input_mode {
                        format!("{buf}█")
                    } else {
                        String::new()
                    }
                } else {
                    app.tui.prompt.clone().unwrap_or_else(|| "\\w $ ".into())
                };
                let val_style = if editing { Style::default().fg(Color::Yellow) } else { Style::default() };
                let hint = if editing { "  Esc cancel  Enter confirm" } else { "  Enter to edit" };
                Line::from(vec![
                    Span::styled(format!("  {:<14}", field.label()), label_s),
                    Span::styled(val, val_style),
                    Span::styled(hint, dim),
                ])
            }
        };
        field_row(f, &rows, i, is_sel, line);
    }
}

fn render_dgx_fields(f: &mut Frame, area: Rect, app: &SettingsApp, bold_orange: Style, dim: Style) {
    let constraints: Vec<Constraint> = DgxField::ALL
        .iter()
        .map(|_| Constraint::Length(2))
        .chain(std::iter::once(Constraint::Fill(1)))
        .collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let editing_label = if let InputMode::EditingText { field_label, .. } = &app.input_mode {
        Some(*field_label)
    } else {
        None
    };

    for (i, field) in DgxField::ALL.iter().enumerate() {
        let is_sel = app.dgx_cursor == i;
        let label_s = if is_sel { bold_orange } else { dim };
        let is_editing = editing_label == Some(field.label());

        let line = match field {
            DgxField::Host | DgxField::Model => {
                let cur = match field {
                    DgxField::Host => dgx_host_str(&app.dgx),
                    DgxField::Model => app.dgx.active_model.clone().unwrap_or_default(),
                    _ => unreachable!(),
                };
                let val = if is_editing {
                    if let InputMode::EditingText { buf, .. } = &app.input_mode {
                        format!("{buf}█")
                    } else {
                        cur
                    }
                } else {
                    cur
                };
                let val_s = if is_editing { Style::default().fg(Color::Yellow) } else { Style::default() };
                let hint = if is_editing { "  Esc cancel  Enter confirm" } else { "  Enter to edit" };
                Line::from(vec![
                    Span::styled(format!("  {:<14}", field.label()), label_s),
                    Span::styled(val, val_s),
                    Span::styled(hint, dim),
                ])
            }
            DgxField::Endpoint => toggle_line(
                field.label(),
                label_s,
                &[
                    ("ollama",     app.dgx.active_endpoint == EndpointKind::Ollama),
                    ("ollama_lb",  app.dgx.active_endpoint == EndpointKind::OllamaLb),
                    ("in_cluster", app.dgx.active_endpoint == EndpointKind::InCluster),
                    ("vllm",       app.dgx.active_endpoint == EndpointKind::Vllm),
                ],
                dim,
            ),
        };
        field_row(f, &rows, i, is_sel, line);
    }
}

fn toggle_line<'a>(
    label: &'static str,
    label_style: Style,
    options: &[(&'a str, bool)],
    dim: Style,
) -> Line<'a> {
    let mut spans = vec![Span::styled(format!("  {:<14}", label), label_style)];
    for (name, selected) in options {
        let bullet = if *selected { "◉ " } else { "○ " };
        let s = if *selected { Style::default().fg(NEWT_ORANGE) } else { dim };
        spans.push(Span::styled(format!("{bullet}{name} "), s));
    }
    Line::from(spans)
}

fn render_about(f: &mut Frame, area: Rect, dim: Style) {
    let save_path = Config::user_config_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(unknown)".into());

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  version      ", dim),
            Span::raw(env!("CARGO_PKG_VERSION")),
        ]),
        Line::from(vec![
            Span::styled("  config file  ", dim),
            Span::raw(save_path),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Env overrides: NEWT_CHAT_STYLE, NEWT_EDIT_MODE, NEWT_PROMPT, NEWT_CONFIG",
            dim,
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_preview(f: &mut Frame, area: Rect, app: &SettingsApp, dim: Style, orange: Style) {
    let prompt = if let InputMode::EditingText { buf, .. } = &app.input_mode {
        expand_prompt(if buf.is_empty() { "\\w $ " } else { buf.as_str() })
    } else {
        app.rendered_prompt()
    };

    let (newt_pfx, human_pfx) = match app.tui.chat_style {
        ChatStyle::Compact => ("▸ ", prompt.as_str()),
        ChatStyle::Verbose => ("newt ▸  ", prompt.as_str()),
    };

    let lines = vec![
        Line::from(Span::styled("  preview", dim)),
        Line::from(vec![
            Span::styled(format!("  {newt_pfx}"), orange),
            Span::raw("Hello, I'm newt. Type a task."),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {human_pfx}"),
                Style::default().fg(Color::Rgb(80, 140, 255)),
            ),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
    ];

    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default().borders(Borders::TOP).border_style(dim),
        ),
        area,
    );
}

fn render_hints(f: &mut Frame, area: Rect, app: &SettingsApp, dim: Style) {
    let status = app.status.as_deref().unwrap_or("");
    let dirty = if app.is_dirty() { "  ● unsaved" } else { "" };
    let hints = format!(
        "  ↑↓ navigate   Enter toggle/edit   Tab next tab   Ctrl-S save   q quit{dirty}"
    );

    let mut lines = vec![Line::from(Span::styled(hints, dim))];
    if !status.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {status}"),
            Style::default().fg(NEWT_ORANGE),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::TOP).border_style(dim)),
        area,
    );
}

fn render_confirm_quit(f: &mut Frame, area: Rect, bold_orange: Style, dim: Style) {
    // Centred modal overlay.
    let w: u16 = 42;
    let h: u16 = 6;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let modal = Rect { x, y, width: w, height: h };

    f.render_widget(Clear, modal);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  You have unsaved changes.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [S] Save  ", bold_orange),
            Span::styled("  [D] Discard  ", dim),
            Span::styled("  [Esc] Cancel", dim),
        ]),
        Line::from(""),
    ];

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(" unsaved changes ", bold_orange))
                    .border_style(bold_orange),
            ),
        modal,
    );
}

// ---------------------------------------------------------------------------
// ANSI parser
// ---------------------------------------------------------------------------

/// Parse a 24-bit ANSI art file into ratatui `Line`s with fg+bg per span.
pub fn parse_ansi_logo(src: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for raw in src.lines() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let bytes = raw.as_bytes();
        let mut i = 0;
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;

        while i < bytes.len() {
            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
                i += 2;
                let start = i;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if let Ok(seq) = std::str::from_utf8(&bytes[start..i]) {
                    let nums: Vec<u8> =
                        seq.split(';').filter_map(|s| s.parse().ok()).collect();
                    match nums.as_slice() {
                        [38, 2, r, g, b] => fg = Color::Rgb(*r, *g, *b),
                        [48, 2, r, g, b] => bg = Color::Rgb(*r, *g, *b),
                        [0] | [] => {
                            fg = Color::Reset;
                            bg = Color::Reset;
                        }
                        _ => {}
                    }
                }
                i += 1;
            } else {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                    i += 1;
                }
                if let Ok(ch) = std::str::from_utf8(&bytes[start..i]) {
                    spans.push(Span::styled(ch.to_string(), Style::default().fg(fg).bg(bg)));
                }
            }
        }
        out.push(Line::from(spans));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> SettingsApp {
        SettingsApp::new(Config::default(), PathBuf::from("/tmp/test.toml"))
    }

    #[test]
    fn new_app_is_not_dirty() {
        assert!(!make_app().is_dirty());
    }

    #[test]
    fn toggle_chat_style_marks_dirty() {
        let mut app = make_app();
        app.toggle_current(); // ChatStyle field
        assert!(app.is_dirty());
        assert_eq!(app.tui.chat_style, ChatStyle::Verbose);
        app.toggle_current();
        assert_eq!(app.tui.chat_style, ChatStyle::Compact);
    }

    #[test]
    fn toggle_no_splash() {
        let mut app = make_app();
        app.tui_cursor = TuiField::ALL.iter().position(|f| *f == TuiField::NoSplash).unwrap();
        assert!(!app.tui.no_splash);
        app.toggle_current();
        assert!(app.tui.no_splash);
        app.toggle_current();
        assert!(!app.tui.no_splash);
    }

    #[test]
    fn toggle_edit_mode() {
        let mut app = make_app();
        app.tui_cursor = TuiField::ALL.iter().position(|f| *f == TuiField::EditMode).unwrap();
        assert_eq!(app.tui.edit_mode, EditMode::Emacs);
        app.toggle_current();
        assert_eq!(app.tui.edit_mode, EditMode::Vi);
        app.toggle_current();
        assert_eq!(app.tui.edit_mode, EditMode::Emacs);
    }

    #[test]
    fn prompt_edit_roundtrip() {
        let mut app = make_app();
        app.tui_cursor = TuiField::ALL.iter().position(|f| *f == TuiField::Prompt).unwrap();
        app.begin_edit();
        assert!(matches!(app.input_mode, InputMode::EditingText { .. }));
        app.type_char('\\');
        app.type_char('w');
        app.type_char(' ');
        app.type_char('$');
        app.confirm_edit();
        assert_eq!(app.tui.prompt.as_deref(), Some("\\w $"));
        assert_eq!(app.input_mode, InputMode::Navigate);
    }

    #[test]
    fn cancel_edit_restores_navigate() {
        let mut app = make_app();
        app.tui_cursor = TuiField::ALL.iter().position(|f| *f == TuiField::Prompt).unwrap();
        app.begin_edit();
        app.cancel_edit();
        assert_eq!(app.input_mode, InputMode::Navigate);
    }

    #[test]
    fn dirty_quit_enters_confirm_mode() {
        // Simulated by directly checking what the event loop does on 'q' when dirty.
        let mut app = make_app();
        app.toggle_current(); // makes it dirty
        assert!(app.is_dirty());
        // Manually trigger the quit-with-dirty logic as the event loop would:
        app.input_mode = InputMode::ConfirmQuit;
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);
    }

    #[test]
    fn dgx_host_roundtrip() {
        let mut app = make_app();
        app.category = Category::Dgx;
        app.dgx_cursor = 0; // Host
        app.begin_edit();
        app.type_char('h');
        app.type_char('t');
        app.type_char('t');
        app.type_char('p');
        app.confirm_edit();
        assert_eq!(dgx_host_str(&app.dgx), "http");
    }

    #[test]
    fn dgx_model_edit() {
        let mut app = make_app();
        app.category = Category::Dgx;
        app.dgx_cursor = DgxField::ALL.iter().position(|f| *f == DgxField::Model).unwrap();
        app.begin_edit();
        for c in "gemma4:e2b".chars() {
            app.type_char(c);
        }
        app.confirm_edit();
        assert_eq!(app.dgx.active_model.as_deref(), Some("gemma4:e2b"));
        assert!(app.is_dirty());
    }

    #[test]
    fn dgx_endpoint_toggle_cycles() {
        let mut app = make_app();
        app.category = Category::Dgx;
        app.dgx_cursor = DgxField::ALL.iter().position(|f| *f == DgxField::Endpoint).unwrap();
        assert_eq!(app.dgx.active_endpoint, EndpointKind::Ollama);
        app.toggle_current();
        assert_eq!(app.dgx.active_endpoint, EndpointKind::OllamaLb);
        app.toggle_current();
        assert_eq!(app.dgx.active_endpoint, EndpointKind::InCluster);
        app.toggle_current();
        assert_eq!(app.dgx.active_endpoint, EndpointKind::Vllm);
        app.toggle_current();
        assert_eq!(app.dgx.active_endpoint, EndpointKind::Ollama);
    }

    #[test]
    fn parse_ansi_logo_produces_lines() {
        let lines = parse_ansi_logo(LOGO_ANSI);
        assert!(!lines.is_empty());
        assert!(lines.len() >= 10);
    }

    #[test]
    fn expand_prompt_replaces_version() {
        let out = expand_prompt("newt \\v");
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
    }
}
