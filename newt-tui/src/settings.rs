//! Interactive settings TUI — `newt settings`.
//!
//! Layout:
//! ```text
//! ┌─ logo (40 cols) ─────┬─ category tabs ──────────────────────────┐
//! │  [LOGO_40 in orange] │  [ TUI ]   DGX   About                   │
//! │                      │  ─────────────────────────────────────    │
//! │                      │  chat style   ◉ compact   ○ verbose       │
//! │                      │  prompt       \w $                        │
//! │                      │  ── preview ──────────────────────────    │
//! │                      │  newt-agent $  _                          │
//! ├──────────────────────┴───────────────────────────────────────────┤
//! │  ↑↓ navigate  Enter toggle/edit  Tab next tab  Ctrl-S save  q   │
//! └──────────────────────────────────────────────────────────────────┘
//! ```

use std::io;
use std::path::{Path, PathBuf};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Tabs},
    Frame, Terminal,
};

use newt_core::{ChatStyle, Config, TuiConfig};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LOGO_PLAIN: &str = include_str!("../../docs/logos/newt-ascii-40.txt");
const LOGO_COLS: u16 = 42; // logo width + 2 border

const NEWT_ORANGE: Color = Color::Rgb(220, 60, 20);
const SEL_BG: Color = Color::Rgb(40, 40, 60);
const DIM: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Tui,
    About,
}

impl Category {
    const ALL: [Self; 2] = [Self::Tui, Self::About];

    fn title(self) -> &'static str {
        match self {
            Self::Tui => "TUI",
            Self::About => "About",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiField {
    ChatStyle,
    Prompt,
}

impl TuiField {
    const ALL: [Self; 2] = [Self::ChatStyle, Self::Prompt];

    fn label(self) -> &'static str {
        match self {
            Self::ChatStyle => "chat style",
            Self::Prompt => "prompt",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum EditMode {
    Navigate,
    EditingText { field: TuiField, buf: String },
}

struct SettingsApp {
    category: Category,
    tui_cursor: usize, // index into TuiField::ALL
    tui: TuiConfig,
    original_tui: TuiConfig,
    edit_mode: EditMode,
    save_path: PathBuf,
    status: Option<String>,
}

impl SettingsApp {
    fn new(config: Config, save_path: PathBuf) -> Self {
        let tui = config.tui.clone().unwrap_or_default();
        Self {
            category: Category::Tui,
            tui_cursor: 0,
            original_tui: tui.clone(),
            tui,
            edit_mode: EditMode::Navigate,
            save_path,
            status: None,
        }
    }

    fn is_dirty(&self) -> bool {
        self.tui != self.original_tui
    }

    fn current_field(&self) -> TuiField {
        TuiField::ALL[self.tui_cursor]
    }

    fn save(&mut self, original_config: &Config) -> anyhow::Result<()> {
        let mut config = original_config.clone();
        config.tui = Some(self.tui.clone());
        config.save(&self.save_path)?;
        self.original_tui = self.tui.clone();
        self.status = Some(format!("Saved → {}", self.save_path.display()));
        Ok(())
    }

    fn toggle_current(&mut self) {
        if self.category == Category::Tui && self.current_field() == TuiField::ChatStyle {
            self.tui.chat_style = self.tui.chat_style.toggle();
            self.status = None;
        }
    }

    fn begin_edit(&mut self) {
        if self.category == Category::Tui && self.current_field() == TuiField::Prompt {
            let buf = self.tui.prompt.clone().unwrap_or_default();
            self.edit_mode = EditMode::EditingText {
                field: TuiField::Prompt,
                buf,
            };
        }
    }

    fn confirm_edit(&mut self) {
        if let EditMode::EditingText { buf, .. } = &self.edit_mode.clone() {
            self.tui.prompt = if buf.is_empty() { None } else { Some(buf.clone()) };
            self.edit_mode = EditMode::Navigate;
            self.status = None;
        }
    }

    fn cancel_edit(&mut self) {
        self.edit_mode = EditMode::Navigate;
    }

    fn type_char(&mut self, c: char) {
        if let EditMode::EditingText { buf, .. } = &mut self.edit_mode {
            buf.push(c);
        }
    }

    fn backspace(&mut self) {
        if let EditMode::EditingText { buf, .. } = &mut self.edit_mode {
            buf.pop();
        }
    }

    fn rendered_prompt(&self) -> String {
        let template = self.tui.prompt.as_deref().unwrap_or("\\w $ ");
        let ws = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "workspace".into());
        let host = hostname();
        let template = template
            .replace("\\W", &std::env::current_dir().unwrap_or_default().to_string_lossy())
            .replace("\\w", &ws)
            .replace("\\h", &host)
            .replace("\\v", env!("CARGO_PKG_VERSION"));
        template
    }
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

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => break,

                Event::Key(KeyEvent {
                    code: KeyCode::Char('s'),
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Err(e) = app.save(original_config) {
                        app.status = Some(format!("Save failed: {e}"));
                    }
                }

                Event::Key(KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                }) if app.edit_mode == EditMode::Navigate => break,

                Event::Key(KeyEvent {
                    code: KeyCode::Tab, ..
                }) if app.edit_mode == EditMode::Navigate => {
                    let idx = (app.category.index() + 1) % Category::ALL.len();
                    app.category = Category::ALL[idx];
                    app.tui_cursor = 0;
                }

                Event::Key(KeyEvent {
                    code: KeyCode::Up, ..
                }) if app.edit_mode == EditMode::Navigate => {
                    if app.category == Category::Tui {
                        app.tui_cursor = app.tui_cursor.saturating_sub(1);
                    }
                }

                Event::Key(KeyEvent {
                    code: KeyCode::Down,
                    ..
                }) if app.edit_mode == EditMode::Navigate => {
                    if app.category == Category::Tui {
                        app.tui_cursor =
                            (app.tui_cursor + 1).min(TuiField::ALL.len().saturating_sub(1));
                    }
                }

                Event::Key(KeyEvent {
                    code: KeyCode::Enter | KeyCode::Char(' '),
                    ..
                }) if app.edit_mode == EditMode::Navigate => match app.category {
                    Category::Tui => match app.current_field() {
                        TuiField::ChatStyle => app.toggle_current(),
                        TuiField::Prompt => app.begin_edit(),
                    },
                    Category::About => {}
                },

                // Text editing
                Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    ..
                }) if app.edit_mode != EditMode::Navigate => app.confirm_edit(),

                Event::Key(KeyEvent {
                    code: KeyCode::Esc, ..
                }) => app.cancel_edit(),

                Event::Key(KeyEvent {
                    code: KeyCode::Backspace,
                    ..
                }) => app.backspace(),

                Event::Key(KeyEvent {
                    code: KeyCode::Char(c),
                    modifiers,
                    ..
                }) if !modifiers.contains(KeyModifiers::CONTROL)
                    && app.edit_mode != EditMode::Navigate =>
                {
                    app.type_char(c);
                }

                Event::Resize(_, _) => {}
                _ => {}
            }
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
    let bold_orange = Style::default()
        .fg(NEWT_ORANGE)
        .add_modifier(Modifier::BOLD);

    // Outer: logo left | main right
    let logo_w = LOGO_COLS.min(area.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(logo_w), Constraint::Fill(1)])
        .split(area);

    // Main: tabs | fields | preview | hints
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs
            Constraint::Fill(1),   // fields
            Constraint::Length(4), // preview
            Constraint::Length(2), // hints (border + text)
        ])
        .split(cols[1]);

    render_logo(f, cols[0], orange);
    render_tabs(f, rows[0], app, bold_orange, dim);
    render_fields(f, rows[1], app, bold_orange, dim);
    render_preview(f, rows[2], app, dim, orange);
    render_hints(f, rows[3], app, dim);
}

fn render_logo(f: &mut Frame, area: Rect, style: Style) {
    let lines: Vec<Line> = LOGO_PLAIN
        .lines()
        .map(|l| Line::from(Span::styled(l.to_owned(), style)))
        .collect();
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn render_tabs(f: &mut Frame, area: Rect, app: &SettingsApp, active: Style, dim: Style) {
    let titles: Vec<Line> = Category::ALL
        .iter()
        .map(|c| Line::from(c.title()))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.category.index())
        .style(dim)
        .highlight_style(active)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .title(Span::styled(" newt settings ", active))
                .border_style(dim),
        );
    f.render_widget(tabs, area);
}

fn render_fields(f: &mut Frame, area: Rect, app: &SettingsApp, bold_orange: Style, dim: Style) {
    let block = Block::default().borders(Borders::NONE);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    match app.category {
        Category::Tui => render_tui_fields(f, area, app, bold_orange, dim),
        Category::About => render_about(f, area, dim),
    }
}

fn render_tui_fields(f: &mut Frame, area: Rect, app: &SettingsApp, bold_orange: Style, dim: Style) {
    let sel_style = Style::default().bg(SEL_BG);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            TuiField::ALL
                .iter()
                .map(|_| Constraint::Length(2))
                .chain(std::iter::once(Constraint::Fill(1)))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, field) in TuiField::ALL.iter().enumerate() {
        let row = rows[i];
        let is_sel = app.tui_cursor == i && app.category == Category::Tui;

        let label_style = if is_sel { bold_orange } else { dim };
        let row_style = if is_sel { sel_style } else { Style::default() };

        let content: Line = match field {
            TuiField::ChatStyle => {
                let opts = [ChatStyle::Compact, ChatStyle::Verbose];
                let mut spans = vec![
                    Span::styled(format!("  {:<14}", field.label()), label_style),
                ];
                for opt in &opts {
                    let bullet = if opt == &app.tui.chat_style { "◉ " } else { "○ " };
                    let s = if opt == &app.tui.chat_style {
                        Style::default().fg(NEWT_ORANGE)
                    } else {
                        dim
                    };
                    spans.push(Span::styled(format!("{bullet}{} ", opt.as_str()), s));
                }
                Line::from(spans)
            }
            TuiField::Prompt => {
                let value = match &app.edit_mode {
                    EditMode::EditingText { buf, .. } => format!("{buf}█"),
                    _ => app
                        .tui
                        .prompt
                        .clone()
                        .unwrap_or_else(|| "\\w $ ".into()),
                };
                let value_style = if matches!(app.edit_mode, EditMode::EditingText { .. }) {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("  {:<14}", field.label()), label_style),
                    Span::styled(value, value_style),
                    if matches!(app.edit_mode, EditMode::EditingText { .. }) {
                        Span::styled("  Esc cancel  Enter confirm", dim)
                    } else {
                        Span::styled("  Enter to edit", dim)
                    },
                ])
            }
        };

        f.render_widget(
            Paragraph::new(content).style(row_style),
            Rect { height: 1, y: row.y + 1, ..row },
        );
    }
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
            "  Env overrides: NEWT_CHAT_STYLE, NEWT_PROMPT, NEWT_CONFIG",
            dim,
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_preview(f: &mut Frame, area: Rect, app: &SettingsApp, dim: Style, orange: Style) {
    let prompt = match &app.edit_mode {
        EditMode::EditingText { buf, .. } => {
            let template = if buf.is_empty() { "\\w $ " } else { buf.as_str() };
            let ws = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "workspace".into());
            template
                .replace("\\w", &ws)
                .replace("\\h", &hostname())
                .replace("\\v", env!("CARGO_PKG_VERSION"))
        }
        _ => app.rendered_prompt(),
    };

    let (newt_prefix, human_prefix) = match app.tui.chat_style {
        ChatStyle::Compact => ("▸ ", prompt.as_str()),
        ChatStyle::Verbose => ("newt ▸  ", prompt.as_str()),
    };

    let lines = vec![
        Line::from(Span::styled("  preview", dim)),
        Line::from(vec![
            Span::styled(format!("  {newt_prefix}"), orange),
            Span::raw("Hello, I'm newt. Type a task."),
        ]),
        Line::from(vec![
            Span::styled(format!("  {human_prefix}"), Style::default().fg(Color::Rgb(80, 140, 255))),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
    ];

    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(dim),
        ),
        area,
    );
}

fn render_hints(f: &mut Frame, area: Rect, app: &SettingsApp, dim: Style) {
    let status = app.status.as_deref().unwrap_or("");
    let dirty = if app.is_dirty() { "  ● unsaved" } else { "" };

    // Always show the key hints; append status or dirty marker after them.
    let hints = format!(
        "  ↑↓ navigate   Enter toggle/edit   Tab next tab   Ctrl-S save   q quit{dirty}"
    );
    let status_line = if !status.is_empty() {
        format!("  {status}")
    } else {
        String::new()
    };

    let mut lines = vec![Line::from(Span::styled(hints, dim))];
    if !status_line.is_empty() {
        lines.push(Line::from(Span::styled(
            status_line,
            Style::default().fg(NEWT_ORANGE),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(dim),
        ),
        area,
    );
}
