//! The scrolling slash-command **palette** (issue #1674) — behind the
//! `rich-tui` feature, Claude Code-style.
//!
//! Typing `/` at an EMPTY prompt on the rich input surface opens a filtering,
//! scrolling list of every dispatchable command above the input line; further
//! characters narrow it (prefix matches first, then substring), ↑/↓ (and
//! `C-p`/`C-n`) move the highlight, Tab/Enter complete the highlighted command
//! into the prompt (Enter completes — it does NOT submit), and Esc closes with
//! the typed text intact. Backspacing the leading `/` closes it.
//!
//! ## One corpus, no drift (three Cs — knowledge in data)
//! The entries are PARSED from [`crate::help_lines`] — the single command
//! corpus `/help` prints — so the palette and `/help` cannot drift apart.
//! There is deliberately no second command list; the parity test below pins
//! that contract.
//!
//! ## Gating (amphibious rule)
//! This module is compiled only under `rich-tui`, and its sole construction
//! site is `RichSurface::event_loop` (rich_input.rs), which chat.rs selects
//! only when `footer_rich_enabled(mode, tty)` holds AND stdout is a real TTY.
//! Lean, piped, and headless runs never construct it — zero behavior change
//! (`docs/decisions/plain_scroller_tui.md`).
//!
//! [`PaletteState`] is PURE (no terminal, no I/O — the `PanelState` pattern
//! from config_panel.rs) and fully unit-tested; [`palette_lines`] is the thin
//! render fn the rich surface draws through its existing inline viewport.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// The palette never grows taller than this many rows (it scrolls instead).
pub(crate) const MAX_VISIBLE: usize = 8;

/// One completable command parsed from a `help_lines()` corpus line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteEntry {
    /// The literal command tokens completion inserts, e.g. `/probe window`.
    pub(crate) cmd: String,
    /// Argument placeholders from the usage, e.g. `<name>` / `[model]`;
    /// empty when the command takes none.
    pub(crate) args: String,
    /// The one-line description from the corpus.
    pub(crate) desc: String,
}

/// Parse `help_lines()`-shaped lines (`"  /cmd args - description"`) into
/// palette entries. Non-slash lines (the `!` escape, key hints, blanks, the
/// trailing tip) are skipped. Alias groups (`/vi  /emacs  /nano`,
/// `/end  /restart`) and pipe groups (`/callers|/callees <sym>`) expand to one
/// entry per slash command so each is findable by prefix. Pure.
pub(crate) fn parse_help_corpus(lines: &[&str]) -> Vec<PaletteEntry> {
    let mut out = Vec::new();
    for line in lines {
        let t = line.trim_start();
        if !t.starts_with('/') {
            continue;
        }
        let Some((usage, desc)) = t.split_once(" - ") else {
            continue;
        };
        let desc = desc.trim().to_string();
        let tokens: Vec<&str> = usage.split_whitespace().collect();
        let Some(first) = tokens.first() else {
            continue;
        };
        let entry = |cmd: &str, args: String| PaletteEntry {
            cmd: cmd.to_string(),
            args,
            desc: desc.clone(),
        };
        if first.contains('|') {
            // "/callers|/callees|… <sym>" — pipe alternatives sharing the args.
            let args = tokens[1..].join(" ");
            out.extend(
                first
                    .split('|')
                    .filter(|alt| alt.starts_with('/'))
                    .map(|alt| entry(alt, args.clone())),
            );
        } else if tokens.iter().filter(|tok| tok.starts_with('/')).count() >= 2 {
            // "/vi  /emacs  /nano" / "/exit  /quit exit quit" — alias groups.
            // Bare (slash-less) aliases aren't typed with `/`, so they're not
            // palette entries.
            out.extend(
                tokens
                    .iter()
                    .filter(|tok| tok.starts_with('/'))
                    .map(|alt| entry(alt, String::new())),
            );
        } else {
            // Bare `|` alternation AFTER the command token ("json|markdown",
            // "… | turn A B | index") is usage notation for alternative
            // ARGUMENT forms — never part of a dispatchable command. When it
            // appears, only the first token is the command and everything
            // after is the argument pattern (review of #1674: parsing
            // "/export json|markdown" as cmd "/export json|markdown" invented
            // a non-dispatchable literal). Pipes INSIDE `<…>`/`[…]` are
            // ordinary placeholders and don't trigger this.
            let bare_alt = tokens[1..].iter().any(|tok| {
                tok.contains('|')
                    && !tok.starts_with('<')
                    && !tok.starts_with('[')
                    && !tok.starts_with('"')
            });
            // Otherwise: "/probe window [model]" — leading literal words are
            // the command; the first `<…>` / `[…]` / quoted token starts the
            // placeholders.
            let split = if bare_alt {
                1
            } else {
                tokens
                    .iter()
                    .position(|tok| !is_literal_word(tok))
                    .unwrap_or(tokens.len())
            };
            out.push(entry(&tokens[..split].join(" "), tokens[split..].join(" ")));
        }
    }
    // Corpus wart (review of #1674): `help_lines()` carries `/search` twice —
    // once in the main list, once in the navigator block. `/help` keeps the
    // corpus verbatim, but the palette shows each COMMAND once: first corpus
    // occurrence wins.
    let mut seen = std::collections::HashSet::new();
    out.retain(|e| seen.insert(e.cmd.clone()));
    out
}

/// A plain command word (starts alphanumeric or `/`) as opposed to an argument
/// placeholder (`<name>`, `[model]`, `"<template>"`, …).
fn is_literal_word(tok: &str) -> bool {
    tok.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '/')
}

/// The palette entries derived from [`crate::help_lines`] — parsed ONCE per
/// process. The single source of truth the palette shares with `/help`.
pub(crate) fn corpus_entries() -> &'static [PaletteEntry] {
    static ENTRIES: std::sync::OnceLock<Vec<PaletteEntry>> = std::sync::OnceLock::new();
    ENTRIES.get_or_init(|| parse_help_corpus(crate::help_lines()))
}

/// The palette's working state: open/closed, the typed filter, the ranked
/// match set, the highlight, and the scroll window. Pure — no terminal, no
/// I/O; every transition is unit-testable (the config_panel `PanelState`
/// pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteState {
    entries: Vec<PaletteEntry>,
    open: bool,
    filter: String,
    /// Indices into `entries`, prefix matches first (corpus order within tier).
    matched: Vec<usize>,
    /// Index into `matched` of the highlighted row.
    highlight: usize,
    /// First visible row of the scroll window (index into `matched`).
    scroll: usize,
    /// Rows the render viewport currently shows (set each frame by the
    /// surface's geometry pass; the scroll window keeps the highlight inside).
    viewport: usize,
}

impl PaletteState {
    pub(crate) fn new(entries: Vec<PaletteEntry>) -> Self {
        Self {
            entries,
            open: false,
            filter: String::new(),
            matched: Vec::new(),
            highlight: 0,
            scroll: 0,
            viewport: 0,
        }
    }

    /// Seed from the shared `help_lines()` corpus.
    pub(crate) fn from_corpus() -> Self {
        Self::new(corpus_entries().to_vec())
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Track an input-buffer edit. Closed → opens ONLY when a `/` was typed at
    /// an empty prompt (`prev == ""`, `now == "/"`). Open → re-filters from the
    /// text after the `/`, and closes when the leading `/` is gone (backspaced
    /// past it, line cleared) or the buffer goes multi-line.
    pub(crate) fn on_buffer_change(&mut self, prev: &str, now: &str) {
        if now == prev {
            return;
        }
        if self.open {
            match now.strip_prefix('/') {
                Some(rest) if !now.contains('\n') => self.set_filter(rest),
                _ => self.close(),
            }
        } else if prev.is_empty() && now == "/" {
            self.open = true;
            self.set_filter("");
        }
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.filter.clear();
        self.matched.clear();
        self.highlight = 0;
        self.scroll = 0;
    }

    /// Re-rank the match set for `filter`. The highlight and scroll window
    /// reset to the top on every change (Claude Code convention). ZERO matches
    /// close the palette outright (review of #1674): an open-but-invisible
    /// palette would keep swallowing ↑/↓/Esc — silently breaking history
    /// recall and vi's Esc-to-NORMAL with no visual cue. Nothing to show means
    /// the palette is not open; the typed text is untouched.
    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.matched = ranked_matches(&self.entries, filter);
        self.highlight = 0;
        self.scroll = 0;
        if self.matched.is_empty() {
            self.close();
        }
    }

    /// Move the highlight up one row, clamped at the top (the config_panel
    /// clamp convention — no wrap).
    pub(crate) fn move_up(&mut self) {
        self.highlight = self.highlight.saturating_sub(1);
        self.ensure_visible();
    }

    /// Move the highlight down one row, clamped at the bottom (no wrap).
    pub(crate) fn move_down(&mut self) {
        if !self.matched.is_empty() {
            self.highlight = (self.highlight + 1).min(self.matched.len() - 1);
        }
        self.ensure_visible();
    }

    /// How many rows the palette wants given `budget` rows of spare terminal
    /// height: the match count, capped by [`MAX_VISIBLE`] and the budget.
    /// 0 when closed (or nothing matches) — the palette simply isn't drawn.
    pub(crate) fn viewport_rows(&self, budget: usize) -> usize {
        if !self.open {
            return 0;
        }
        self.matched.len().min(MAX_VISIBLE).min(budget)
    }

    /// Record the rows the surface will actually draw and re-clamp the scroll
    /// window so the highlight stays visible.
    pub(crate) fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows;
        self.ensure_visible();
    }

    pub(crate) fn viewport(&self) -> usize {
        self.viewport
    }

    /// Slide the scroll window so `scroll <= highlight < scroll + viewport`,
    /// clamped to the list.
    fn ensure_visible(&mut self) {
        if self.viewport == 0 {
            self.scroll = 0;
            return;
        }
        if self.highlight < self.scroll {
            self.scroll = self.highlight;
        } else if self.highlight >= self.scroll + self.viewport {
            self.scroll = self.highlight + 1 - self.viewport;
        }
        self.scroll = self
            .scroll
            .min(self.matched.len().saturating_sub(self.viewport));
    }

    /// The text completing the highlighted command puts in the prompt: the
    /// command itself, plus a trailing space when it takes arguments (so the
    /// operator can keep typing them). `None` when closed or nothing matches
    /// (the caller lets the key fall through).
    pub(crate) fn completion(&self) -> Option<String> {
        if !self.open {
            return None;
        }
        let e = &self.entries[*self.matched.get(self.highlight)?];
        Some(if e.args.is_empty() {
            e.cmd.clone()
        } else {
            format!("{} ", e.cmd)
        })
    }
}

/// Rank entries for `filter` (the text after the `/`), case-insensitively:
/// command-prefix matches first, then substring hits anywhere in the command,
/// its args, or its description (so aliases mentioned in descriptions — e.g.
/// `/compact` under `/compress` — are still findable). Corpus order within
/// each tier. Pure.
fn ranked_matches(entries: &[PaletteEntry], filter: &str) -> Vec<usize> {
    let f = filter.to_lowercase();
    if f.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut out: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.cmd[1..].to_lowercase().starts_with(&f))
        .map(|(i, _)| i)
        .collect();
    for (i, e) in entries.iter().enumerate() {
        if out.contains(&i) {
            continue;
        }
        let hay = format!("{} {} {}", &e.cmd[1..], e.args, e.desc).to_lowercase();
        if hay.contains(&f) {
            out.push(i);
        }
    }
    out
}

/// What the event loop should do with a key after the palette has seen it —
/// the PURE key-interception decision (review of #1674), so the loop's
/// contracts (Enter completes but never submits; the palette owns ↑/↓ before
/// history recall; Esc is swallowed; a closed palette touches nothing) are
/// unit-testable without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaletteStep {
    /// The palette consumed the key; the loop moves to the next event.
    Swallowed,
    /// Replace the input buffer with this completion (the palette closed).
    /// This is a COMPLETION, never a submit.
    CompleteTo(String),
    /// The palette is closed or uninterested; the key flows on to history
    /// recall and the editor unchanged.
    PassThrough,
}

/// Feed one key press to the palette. Open, it owns navigation (↑/↓,
/// `C-p`/`C-n`), Esc (close, typed text intact), Tab (complete; swallowed
/// even with nothing to complete, so a literal tab never lands in a slash
/// line), and plain Enter (complete — NOT submit; Shift-Enter passes through
/// as the editor's newline). Closed, every key passes through untouched.
pub(crate) fn palette_step(state: &mut PaletteState, key: &KeyEvent) -> PaletteStep {
    if !state.is_open() {
        return PaletteStep::PassThrough;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up => {
            state.move_up();
            PaletteStep::Swallowed
        }
        KeyCode::Down => {
            state.move_down();
            PaletteStep::Swallowed
        }
        KeyCode::Char('p') if ctrl => {
            state.move_up();
            PaletteStep::Swallowed
        }
        KeyCode::Char('n') if ctrl => {
            state.move_down();
            PaletteStep::Swallowed
        }
        KeyCode::Esc => {
            state.close();
            PaletteStep::Swallowed
        }
        KeyCode::Tab => match state.completion() {
            Some(text) => {
                state.close();
                PaletteStep::CompleteTo(text)
            }
            None => {
                state.close();
                PaletteStep::Swallowed
            }
        },
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            match state.completion() {
                Some(text) => {
                    state.close();
                    PaletteStep::CompleteTo(text)
                }
                // Defensive: an open palette always has a highlight now that
                // zero matches auto-close, but if there is ever nothing to
                // complete, Enter closes and falls through to the normal
                // submit path.
                None => {
                    state.close();
                    PaletteStep::PassThrough
                }
            }
        }
        _ => PaletteStep::PassThrough,
    }
}

/// Render the visible window as styled rows — the thin presentation layer over
/// [`PaletteState`]. `❯` marks the highlight; commands align in a column with
/// their descriptions dimmed beside them. Empty when closed or the viewport is
/// zero. Drawn by the rich surface inside its existing inline region — no
/// second event loop, no new raw-mode surface.
pub(crate) fn palette_lines(state: &PaletteState) -> Vec<Line<'static>> {
    if !state.open || state.viewport == 0 {
        return Vec::new();
    }
    // Align descriptions in one column: pad to the widest `cmd args` head
    // across the whole match set (stable while scrolling), capped so one very
    // long usage can't push every description off-screen.
    let head_w = |e: &PaletteEntry| {
        e.cmd.chars().count()
            + if e.args.is_empty() {
                0
            } else {
                e.args.chars().count() + 1
            }
    };
    let col = state
        .matched
        .iter()
        .map(|&i| head_w(&state.entries[i]))
        .max()
        .unwrap_or(0)
        .min(36);
    // Through the role table. This line was `Color::Rgb(255, 165, 90)` — the
    // literal value of `ACTIVE_INPUT_CT`, written out a second time. The same
    // duplication was fixed in `header_line` (#2019) and reappeared here,
    // because nothing NAMED the colour.
    let accent = crate::theme::color(crate::theme::Role::Accent);
    let end = (state.scroll + state.viewport).min(state.matched.len());
    state.matched[state.scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, &idx)| {
            let e = &state.entries[idx];
            let hl = state.scroll + offset == state.highlight;
            let (marker, cmd_style, desc_style) = if hl {
                (
                    Span::styled("❯ ", Style::default().fg(accent)),
                    Style::default()
                        .fg(crate::theme::color(crate::theme::Role::Emphasis))
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(crate::theme::color(crate::theme::Role::Muted)),
                )
            } else {
                (
                    Span::raw("  "),
                    Style::default(),
                    Style::default().fg(crate::theme::color(crate::theme::Role::Dim)),
                )
            };
            let mut spans = vec![marker, Span::styled(e.cmd.clone(), cmd_style)];
            if !e.args.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", e.args),
                    Style::default().fg(crate::theme::color(crate::theme::Role::Dim)),
                ));
            }
            let pad = col.saturating_sub(head_w(e)) + 2;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(e.desc.clone(), desc_style));
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small fixture corpus in the exact `help_lines()` shape, covering the
    /// parse cases: plain command, subcommand, `<>`/`[]` placeholders, alias
    /// groups, pipe groups, and non-slash lines that must be skipped.
    fn fixture() -> Vec<PaletteEntry> {
        parse_help_corpus(&[
            "  /models                  - list models on the active endpoint",
            "  /models capabilities     - tool-conformance matrix (cached)",
            "  /model <name>            - switch model on the active backend",
            "  /mode [name]             - show/set operating style",
            "  /compress [focus]        - compress context now (alias: /compact)",
            "  /vi  /emacs  /nano       - switch line-editor key bindings",
            "  /callers|/callees <sym>  - graph queries",
            "  /version                 - print newt version",
            "  ! <command>              - host escape (not a slash command)",
            "",
            "  Add --help (or -h) to any command for its detail page.",
        ])
    }

    fn open_palette(filter_text: &str) -> PaletteState {
        let mut s = PaletteState::new(fixture());
        s.on_buffer_change("", "/");
        if !filter_text.is_empty() {
            s.on_buffer_change("/", &format!("/{filter_text}"));
        }
        s
    }

    fn matched_cmds(s: &PaletteState) -> Vec<&str> {
        s.matched
            .iter()
            .map(|&i| s.entries[i].cmd.as_str())
            .collect()
    }

    #[test]
    fn parse_extracts_commands_args_and_descriptions() {
        let e = fixture();
        // 6 single-command lines + a 3-way alias group + a 2-way pipe group;
        // the `!` escape, the blank, and the tip line parse to nothing.
        assert_eq!(e.len(), 11);
        assert!(e.iter().all(|x| x.cmd.starts_with('/')));
        assert_eq!((e[0].cmd.as_str(), e[0].args.as_str()), ("/models", ""));
        assert_eq!(e[1].cmd, "/models capabilities");
        assert_eq!(
            (e[2].cmd.as_str(), e[2].args.as_str()),
            ("/model", "<name>")
        );
        assert_eq!(e[2].desc, "switch model on the active backend");
        assert_eq!((e[3].cmd.as_str(), e[3].args.as_str()), ("/mode", "[name]"));
        // Alias group → one entry per slash command, same description.
        let alias: Vec<_> = e
            .iter()
            .filter(|x| x.desc.contains("key bindings"))
            .collect();
        assert_eq!(
            alias.iter().map(|x| x.cmd.as_str()).collect::<Vec<_>>(),
            vec!["/vi", "/emacs", "/nano"]
        );
        // Pipe group → one entry per command, sharing the args.
        let graph: Vec<_> = e.iter().filter(|x| x.desc == "graph queries").collect();
        assert_eq!(
            graph.iter().map(|x| x.cmd.as_str()).collect::<Vec<_>>(),
            vec!["/callers", "/callees"]
        );
        assert!(graph.iter().all(|x| x.args == "<sym>"));
    }

    /// The parity contract (#1674): the palette IS the `help_lines()` corpus.
    /// Every slash line parses to at least one entry, and structural
    /// invariants pin that parsing never INVENTS a command: a parsed cmd is
    /// `/`-led literal words only — no `|` alternation, no placeholder
    /// characters, no second `/`-command glued on. (Multi-word cmds like
    /// `/models capabilities` are deliberate subcommand completions.)
    #[test]
    fn corpus_parity_every_slash_line_parses_and_no_command_is_invented() {
        let lines = crate::help_lines();
        let entries = corpus_entries();
        assert!(!entries.is_empty(), "the real corpus yields entries");
        // corpus_entries derives from help_lines — the single source of truth.
        assert_eq!(entries, parse_help_corpus(lines).as_slice());
        // Every slash line parses (in isolation, so the /search dedupe can't
        // mask a line that stopped parsing).
        for line in lines {
            if line.trim_start().starts_with('/') {
                assert!(
                    !parse_help_corpus(&[line]).is_empty(),
                    "corpus slash line failed to parse: {line:?}"
                );
            }
        }
        // The "nothing invents commands" pin: every completable cmd is made
        // of plain `/`-led literal words, with args/alternation kept out.
        for e in entries {
            let mut toks = e.cmd.split(' ');
            let first = toks.next().unwrap_or("");
            assert!(
                first.starts_with('/') && first.len() > 1,
                "cmd must start with a /command token: {e:?}"
            );
            for tok in e.cmd.split(' ').skip(1) {
                assert!(
                    tok.chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric()),
                    "subcommand tokens are plain words: {e:?}"
                );
            }
            for banned in ['|', '<', '[', '"'] {
                assert!(
                    !e.cmd.contains(banned),
                    "cmd contains usage notation ({banned:?}) — an invented, \
                     non-dispatchable literal: {e:?}"
                );
            }
            assert!(
                !e.desc.is_empty(),
                "every entry keeps its description: {e:?}"
            );
        }
    }

    /// Review finding 1 (#1674): bare `|` alternation in NON-first tokens is
    /// usage notation for argument forms, never command text. Replays the
    /// real corpus rows that carry it and pins their EXACT (cmd, args), plus
    /// the bracketed-pipe rows that must NOT trigger the rule.
    #[test]
    fn corpus_replay_bare_pipe_alternation_stays_out_of_commands() {
        let pin = |cmd: &str| {
            corpus_entries()
                .iter()
                .find(|e| e.cmd == cmd)
                .unwrap_or_else(|| panic!("no corpus entry for {cmd}"))
                .args
                .clone()
        };
        // "json|markdown" is two argument forms of plain /export — the old
        // parse invented the non-dispatchable literal "/export json|markdown".
        assert_eq!(pin("/export"), "json|markdown");
        // The /compare row is three alternative argument forms of /compare.
        assert_eq!(pin("/compare"), "semantic lexical | turn A B | index");
        // Pipes INSIDE `[…]`/`<…>` are ordinary placeholders: subcommand cmds
        // survive and the placeholder stays in args.
        assert_eq!(
            pin("/context compaction"),
            "[headroom_aware|message_count|reset]"
        );
        // #1667 gave `/backend` a BARE panel row that now leads the corpus, and
        // the palette dedupes by command with the first occurrence winning (see
        // the test below) — so the palette's `/backend` is the argument-less
        // panel form. The pipe rule itself still governs the text row, pinned
        // directly here so #1674's finding stays covered by a real corpus line.
        assert_eq!(pin("/backend"), "");
        let text_form = parse_help_corpus(&[
            "  /backend <openai|ollama> [model] - text form: switch the wire kind",
        ]);
        assert_eq!(text_form[0].cmd, "/backend");
        assert_eq!(text_form[0].args, "<openai|ollama> [model]");
        // The pipe-GROUP rule (first token) still expands alternatives.
        assert_eq!(pin("/callers"), "<sym>");
        assert_eq!(pin("/hierarchy"), "<sym>");
        // Quoted placeholder ends the command.
        assert_eq!(pin("/prompt set"), "\"<template>\"");
        // Review finding 6: /help is a dispatchable, completable entry.
        assert_eq!(pin("/help"), "[command]");
    }

    /// Review finding 8 (#1674): `help_lines()` carries `/search` twice (a
    /// corpus wart kept verbatim for `/help`); the palette dedupes by command,
    /// first occurrence winning.
    #[test]
    fn duplicate_corpus_commands_collapse_to_the_first_occurrence() {
        let dupes = parse_help_corpus(&[
            "  /search <query>          - semantic code search",
            "  /search [query|preview]  - the same cockpit, second listing",
        ]);
        assert_eq!(dupes.len(), 1, "one row per command");
        assert_eq!(dupes[0].args, "<query>", "first occurrence wins");
        // And on the real corpus: exactly one /search entry, the first row's.
        let searches: Vec<_> = corpus_entries()
            .iter()
            .filter(|e| e.cmd == "/search")
            .collect();
        assert_eq!(searches.len(), 1, "the duplicated corpus row is deduped");
        assert_eq!(searches[0].args, "<query>");
    }

    #[test]
    fn opens_only_when_slash_is_typed_at_an_empty_prompt() {
        let mut s = PaletteState::new(fixture());
        assert!(!s.is_open());
        s.on_buffer_change("", "x");
        assert!(!s.is_open(), "ordinary typing never opens it");
        s.on_buffer_change("x", "x/");
        assert!(!s.is_open(), "a mid-line slash never opens it");
        s.on_buffer_change("x", "");
        s.on_buffer_change("", "/model x");
        assert!(!s.is_open(), "a pasted command line is not a typed `/`");
        s.on_buffer_change("", "/");
        assert!(s.is_open(), "`/` at an empty prompt opens the palette");
        // An empty filter lists the whole corpus.
        assert_eq!(s.matched.len(), s.entries.len());
        assert_eq!(s.highlight, 0);
    }

    #[test]
    fn typing_filters_prefix_matches_before_substring_matches() {
        let s = open_palette("mo");
        // Prefix tier in corpus order; nothing else in the fixture has "mo".
        assert_eq!(
            matched_cmds(&s),
            vec!["/models", "/models capabilities", "/model", "/mode"]
        );
        // A description-only hit lands via the substring tier: "/compact" is
        // an alias mentioned only in /compress's description.
        let s = open_palette("compact");
        assert_eq!(matched_cmds(&s), vec!["/compress"]);
    }

    #[test]
    fn refiltering_resets_the_highlight_and_scroll() {
        let mut s = open_palette("");
        s.set_viewport(3);
        for _ in 0..5 {
            s.move_down();
        }
        assert!(s.highlight > 0 && s.scroll > 0);
        s.on_buffer_change("/", "/mo");
        assert_eq!((s.highlight, s.scroll), (0, 0));
    }

    #[test]
    fn highlight_clamps_at_both_ends_no_wrap() {
        let mut s = open_palette("");
        let last = s.matched.len() - 1;
        for _ in 0..s.matched.len() + 5 {
            s.move_down();
        }
        assert_eq!(s.highlight, last, "clamped at the bottom, no wrap");
        for _ in 0..s.matched.len() + 5 {
            s.move_up();
        }
        assert_eq!(s.highlight, 0, "clamped at the top, no wrap");
    }

    #[test]
    fn scroll_window_keeps_the_highlight_visible() {
        let mut s = open_palette(""); // 11 matches
        s.set_viewport(3);
        assert_eq!(s.scroll, 0);
        for _ in 0..3 {
            s.move_down(); // highlight 3 — just past the 0..3 window
        }
        assert_eq!(s.scroll, 1, "window slides down to keep the highlight");
        for _ in 0..20 {
            s.move_down();
        }
        assert_eq!(s.highlight, 10);
        assert_eq!(s.scroll, 8, "window pinned to the tail");
        for _ in 0..20 {
            s.move_up();
        }
        assert_eq!((s.highlight, s.scroll), (0, 0), "window slides back up");
        // Shrinking the viewport re-clamps the window around the highlight.
        for _ in 0..20 {
            s.move_down();
        }
        s.set_viewport(2);
        assert_eq!(s.scroll, 9, "re-clamped so the highlight stays visible");
    }

    #[test]
    fn viewport_rows_cap_at_max_visible_matches_and_budget() {
        let closed = PaletteState::new(fixture());
        assert_eq!(closed.viewport_rows(100), 0, "closed → no rows");
        let s = open_palette(""); // 11 matches
        assert_eq!(s.viewport_rows(100), MAX_VISIBLE, "capped at MAX_VISIBLE");
        assert_eq!(s.viewport_rows(3), 3, "capped by the terminal budget");
        let s = open_palette("compact"); // 1 match
        assert_eq!(s.viewport_rows(100), 1, "capped by the match count");
        let s = open_palette("zzzz"); // 0 matches → auto-closed
        assert_eq!(s.viewport_rows(100), 0, "nothing to show");
    }

    /// Review finding 2 (#1674): a zero-match filter CLOSES the palette —
    /// never an invisible open state that keeps swallowing keys. Closed, ↑
    /// passes through (history recall works right after typing an unknown
    /// "/…zzz") and Esc passes through (vi reaches NORMAL).
    #[test]
    fn zero_matches_auto_close_so_keys_reach_history_and_the_editor() {
        let mut s = open_palette("mo");
        assert!(s.is_open());
        s.on_buffer_change("/mo", "/mozzz");
        assert!(!s.is_open(), "zero matches → closed, not invisibly open");
        for code in [KeyCode::Up, KeyCode::Esc, KeyCode::Enter] {
            assert_eq!(
                palette_step(&mut s, &KeyEvent::new(code, KeyModifiers::NONE)),
                PaletteStep::PassThrough,
                "closed palette must not swallow {code:?}"
            );
        }
    }

    /// Review finding 4 (#1674): the event-loop interception contracts, unit-
    /// tested through the pure `palette_step` seam the loop dispatches on.
    #[test]
    fn palette_step_owns_navigation_completion_and_esc_while_open() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);

        // Palette-before-history: while open, ↑/↓ (and C-p/C-n) are consumed
        // and move the HIGHLIGHT — they can never reach history recall.
        let mut s = open_palette("");
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Down)),
            PaletteStep::Swallowed
        );
        assert_eq!(s.highlight, 1);
        assert_eq!(palette_step(&mut s, &ctrl('n')), PaletteStep::Swallowed);
        assert_eq!(s.highlight, 2);
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Up)),
            PaletteStep::Swallowed
        );
        assert_eq!(palette_step(&mut s, &ctrl('p')), PaletteStep::Swallowed);
        assert_eq!(s.highlight, 0);

        // Enter COMPLETES the highlighted command — never a submit
        // (CompleteTo, not PassThrough) — and the palette closes.
        let mut s = open_palette("model");
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Enter)),
            PaletteStep::CompleteTo("/models".to_string())
        );
        assert!(!s.is_open());

        // Shift-Enter stays the editor's newline (the buffer sync then
        // closes on the multi-line buffer).
        let mut s = open_palette("model");
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(palette_step(&mut s, &shift_enter), PaletteStep::PassThrough);

        // Tab completes too, and is swallowed rather than typing a literal
        // tab into the slash line.
        let mut s = open_palette("model");
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Tab)),
            PaletteStep::CompleteTo("/models".to_string())
        );

        // Esc is swallowed (closes the palette; the editor never sees it, so
        // vi stays in INSERT with the typed text intact)…
        let mut s = open_palette("mo");
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Esc)),
            PaletteStep::Swallowed
        );
        assert!(!s.is_open());

        // …and ordinary typing passes through to the editor (the buffer sync
        // does the filtering).
        let mut s = open_palette("mo");
        assert_eq!(
            palette_step(&mut s, &key(KeyCode::Char('d'))),
            PaletteStep::PassThrough
        );

        // Closed: EVERY interception key passes through untouched.
        let mut s = PaletteState::new(fixture());
        for k in [
            key(KeyCode::Up),
            key(KeyCode::Down),
            key(KeyCode::Tab),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            ctrl('p'),
            ctrl('n'),
        ] {
            assert_eq!(palette_step(&mut s, &k), PaletteStep::PassThrough);
        }
    }

    /// Recalled history lines and multi-character type-ahead prefills are not
    /// "typing `/` at an empty prompt": only a lone `/` opens. (Review
    /// finding 5: the same sync now runs over the type-ahead prefill, so a
    /// prefilled lone `/` opens exactly like a live keypress.)
    #[test]
    fn recalled_or_prefilled_full_lines_never_open_but_a_lone_slash_does() {
        let mut s = PaletteState::new(fixture());
        s.on_buffer_change("", "/version");
        assert!(
            !s.is_open(),
            "a recalled/prefilled command line stays closed"
        );
        s.on_buffer_change("/version", "");
        s.on_buffer_change("", "/");
        assert!(s.is_open(), "a prefilled lone `/` opens like a keypress");
    }

    #[test]
    fn completion_inserts_the_command_with_a_space_when_it_takes_args() {
        let mut s = open_palette("model");
        // Prefix tier: /models, /models capabilities, /model.
        assert_eq!(s.completion().as_deref(), Some("/models"));
        s.move_down();
        s.move_down();
        assert_eq!(
            s.completion().as_deref(),
            Some("/model "),
            "an argful command completes with a trailing space"
        );
        let s = open_palette("zzzz");
        assert_eq!(s.completion(), None, "no match → nothing to complete");
        let mut s = open_palette("model");
        s.close();
        assert_eq!(s.completion(), None, "closed → nothing to complete");
    }

    #[test]
    fn backspacing_past_the_slash_or_leaving_slash_form_closes() {
        let mut s = open_palette("mo");
        s.on_buffer_change("/mo", "/m");
        assert!(s.is_open(), "still a slash line — narrower filter");
        s.on_buffer_change("/m", "/");
        assert!(s.is_open(), "back to the full list");
        s.on_buffer_change("/", "");
        assert!(!s.is_open(), "backspacing the leading `/` closes it");
        let mut s = open_palette("m");
        s.on_buffer_change("/m", "m");
        assert!(!s.is_open(), "losing the leading `/` closes it");
        let mut s = open_palette("");
        s.on_buffer_change("/", "/a\nb");
        assert!(!s.is_open(), "a multi-line buffer closes it");
    }

    #[test]
    fn esc_close_leaves_state_reusable_and_does_not_reopen_until_fresh_slash() {
        let mut s = open_palette("mo");
        s.close();
        assert!(!s.is_open());
        assert_eq!(s.viewport_rows(100), 0);
        // Continuing to type after Esc must NOT reopen it…
        s.on_buffer_change("/mo", "/mod");
        assert!(!s.is_open(), "Esc means closed until a fresh `/`");
        // …but a fresh `/` at an empty prompt does.
        s.on_buffer_change("/mod", "");
        s.on_buffer_change("", "/");
        assert!(s.is_open());
    }

    #[test]
    fn render_shows_the_window_and_marks_the_highlight() {
        let mut s = open_palette("");
        s.set_viewport(3);
        for _ in 0..4 {
            s.move_down(); // highlight 4, scroll 2 → window rows 2,3,4
        }
        let lines = palette_lines(&s);
        assert_eq!(lines.len(), 3, "exactly the viewport rows render");
        let text = |l: &Line| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
        assert!(
            text(&lines[0]).contains("/model"),
            "window starts at scroll"
        );
        assert!(
            text(&lines[2]).starts_with("❯ ") && text(&lines[2]).contains("/compress"),
            "the highlighted row wears the chevron: {:?}",
            text(&lines[2])
        );
        assert!(
            !text(&lines[0]).starts_with("❯ ") && !text(&lines[1]).starts_with("❯ "),
            "only the highlight wears it"
        );
        assert!(
            text(&lines[2]).contains("compress context now"),
            "descriptions ride beside the commands"
        );
        // Closed / zero-viewport palettes render nothing.
        s.close();
        assert!(palette_lines(&s).is_empty());
    }

    /// #1674 gating: the palette's ONLY construction site is the rich
    /// surface's event loop, selected by `rich_surface_selected` in chat.rs —
    /// compiled only under `rich-tui` (this module doesn't exist otherwise).
    /// Review finding 7: `footer_rich_enabled` ALONE does not protect a piped
    /// run — `FooterMode::On` forces the rich PROMPT STRING even off a TTY
    /// (it lands in logfiles by design). What protects piped/headless is the
    /// TTY conjunction in [`crate::prompt::rich_surface_selected`], pinned
    /// here across the whole mode × TTY matrix.
    #[test]
    fn gating_is_the_tty_conjunction_not_the_footer_predicate_alone() {
        use crate::prompt::{footer_rich_enabled, rich_surface_selected};
        use newt_core::FooterMode;
        // The footer predicate alone would let a forced-on footer through a
        // pipe — a rich prompt STRING is fine in a logfile…
        assert!(footer_rich_enabled(FooterMode::On, false));
        // …but the rich SURFACE (and with it the palette) is refused off a
        // TTY in every mode:
        assert!(!rich_surface_selected(FooterMode::On, false));
        assert!(!rich_surface_selected(FooterMode::Auto, false));
        assert!(!rich_surface_selected(FooterMode::Off, false));
        // On a TTY, the footer mode decides (Off = --plain stays lean):
        assert!(rich_surface_selected(FooterMode::On, true));
        assert!(rich_surface_selected(FooterMode::Auto, true));
        assert!(!rich_surface_selected(FooterMode::Off, true));
    }
}
