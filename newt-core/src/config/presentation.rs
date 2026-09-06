//! Terminal presentation modes and session display overrides.

use serde::{Deserialize, Serialize};

use super::Config;

/// Prompt richness — the `[tui] footer` key. Selects the *default* prompt
/// template when `[tui] prompt` is unset; an explicit `[tui] prompt` always
/// wins. The rich default folds a timestamp + status into the prompt line
/// itself (`[<ts> · <model> · <ws> · <mode> ] ❯ `), so the input surface floats
/// it at the bottom while idle (like cargo's progress line) and it doubles as a
/// greppable per-turn log marker — no region, no cursor games.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FooterMode {
    /// Rich default prompt on a TTY, plain `\w $ ` otherwise (the default).
    /// The amphibious choice: decorated on a human terminal, bare in pipes /
    /// `newt worker` / the wyvern deep-cut.
    #[default]
    Auto,
    /// Always use the rich default prompt (even off a TTY — screenshots, tests).
    On,
    /// Always use the plain bare prompt. Equivalent to `--plain`.
    Off,
}

/// Color / theme mode — the `[tui] color` key and the `--color` CLI flag
/// (issue #527). Selects whether — and eventually how — ANSI color is emitted
/// for the interactive prompt and chat surface. The default is `auto`: color on
/// a TTY, none in pipes / under `NO_COLOR` / `TERM=dumb`.
///
/// `dark`/`light`/`inverted`/`minimal` are accepted and parse today; their
/// palettes are initial mappings (currently the chromatic default) tuned in a
/// later pass. The terminal-aware *resolution* lives in the TUI layer — newt-core
/// has no business probing the terminal — so this enum only exposes the pure
/// pieces ([`from_keyword`](Self::from_keyword) / [`keyword`](Self::keyword) /
/// [`forced`](Self::forced) / [`is_mono`](Self::is_mono)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Color on a TTY; none off one or under `NO_COLOR`/`TERM=dumb` (default).
    #[default]
    Auto,
    /// Always emit color — even off a TTY (screenshots, captured logs). An
    /// explicit `--color=always` also overrides `NO_COLOR` (documented deviation).
    Always,
    /// Never emit color.
    Never,
    /// Reduced color: structure only, no bright accents. (Initial mapping:
    /// chromatic; tuned later.)
    Minimal,
    /// Swapped foreground/background accents for high-contrast terminals.
    /// (Initial mapping: chromatic; tuned later.)
    Inverted,
    /// Palette tuned for a dark background — the current chromatic default.
    Dark,
    /// Palette tuned for a light background. (Initial mapping: chromatic; tuned later.)
    Light,
    /// Force monochrome — no color, ASCII glyph fallbacks. Equivalent to `--mono`.
    Mono,
}

impl ColorMode {
    /// Parse a CLI/config keyword (case-insensitive) into a mode. `on`/`off` are
    /// accepted as aliases of `always`/`never`; `monochrome` aliases `mono`.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" | "on" => Some(Self::Always),
            "never" | "off" => Some(Self::Never),
            "minimal" => Some(Self::Minimal),
            "inverted" => Some(Self::Inverted),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "mono" | "monochrome" => Some(Self::Mono),
            _ => None,
        }
    }

    /// The canonical lowercase keyword for this mode (round-trips `from_keyword`
    /// and matches the serde representation).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
            Self::Minimal => "minimal",
            Self::Inverted => "inverted",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Mono => "mono",
        }
    }

    /// Whether this mode forces a color decision regardless of the terminal:
    /// `Some(true)` = force color on, `Some(false)` = force off, `None` = defer
    /// to terminal detection (`Auto`).
    pub fn forced(self) -> Option<bool> {
        match self {
            Self::Always | Self::Minimal | Self::Inverted | Self::Dark | Self::Light => Some(true),
            Self::Never | Self::Mono => Some(false),
            Self::Auto => None,
        }
    }

    /// Whether color is fully disabled in monochrome form. `Mono` additionally
    /// signals ASCII-glyph fallbacks (`>` for `❯`) to callers; `Never` just
    /// drops color.
    pub fn is_mono(self) -> bool {
        matches!(self, Self::Mono)
    }
}

/// Markdown rendering mode — the `[tui] markdown` key and the `/markdown`
/// command (Step 25.4, #568). This controls RichTUI text output, including
/// assistant replies and built-in Markdown documents such as `/help`. `Auto`
/// renders Markdown whenever color is active; `On`/`Off` force the choice
/// (`On` still needs color to emit ANSI). The effective decision is
/// `mode.forced().unwrap_or(color_on) && color_on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownMode {
    /// Render Markdown whenever color is active (default).
    #[default]
    Auto,
    /// Force Markdown rendering on (still gated by color support).
    On,
    /// Disable Markdown rendering — stream raw text.
    Off,
}

/// The Markdown display mode this session resolves to: the operator's session
/// override, else `[tui] markdown`, else the default.
///
/// # Why this exists (#2009 PR4)
///
/// The override was a `run_chat` LOCAL (`markdown_override: Option<bool>`),
/// which is the precondition §5 names: **a receipt writer cannot read a
/// local.** `settings_form::apply` is a pure function reached from a form, a
/// deep link and a shim; none of them can see into the session loop. So the
/// override moves to where every other absorbed display setting already keeps
/// it — a process-global under the #1850 lock, read through ONE resolver that
/// owns the precedence.
///
/// Deliberately the same shape as `agentic::thinking_mode`, not a new one. Two
/// resolvers for "session override, else config, else default" is how they
/// come to disagree.
#[must_use]
pub fn session_markdown_mode() -> MarkdownMode {
    if let Some(mode) = std::env::var("NEWT_MARKDOWN")
        .ok()
        .as_deref()
        .and_then(MarkdownMode::from_keyword)
    {
        return mode;
    }
    Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.markdown)
        .unwrap_or_default()
}

/// Whether the operator has pinned the mode this session, as opposed to
/// inheriting it from config. `/markdown` reports which, and a receipt's
/// from→to is meaningless without it.
#[must_use]
pub fn markdown_is_session_pinned() -> bool {
    std::env::var("NEWT_MARKDOWN")
        .ok()
        .as_deref()
        .and_then(MarkdownMode::from_keyword)
        .is_some()
}

impl MarkdownMode {
    /// Parse a CLI/config/command keyword (case-insensitive). `always`/`never`
    /// alias `on`/`off`.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" | "always" => Some(Self::On),
            "off" | "never" => Some(Self::Off),
            _ => None,
        }
    }

    /// The canonical lowercase keyword (round-trips `from_keyword` + serde).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// `Some(true)`/`Some(false)` force the decision; `None` (`Auto`) defers to
    /// color detection.
    pub fn forced(self) -> Option<bool> {
        match self {
            Self::On => Some(true),
            Self::Off => Some(false),
            Self::Auto => None,
        }
    }
}

/// The operator's session spill-detail override, if one is installed.
///
/// `None` follows `[tui] spill_lines`; `Some(0)` is this knob's **unbounded**;
/// `Some(n)` keeps `n` rows.
///
/// # Why this exists (#2009 PR7b)
///
/// It was `spill_lines_override`, a `run_chat` local shared by `/spill` and
/// `/detail` — deliberately one variable, "so the launch flag and the runtime
/// control cannot disagree". That property is preserved and widened: the
/// `/settings detail` field is a THIRD door onto the same knob, and it had to
/// be able to see the value, which §5's precondition says a pure function
/// cannot do while the value is a local.
///
/// The launch flag (`--trace`) installs its value here at startup, so "one
/// variable" still holds across all three doors.
#[must_use]
pub fn session_spill_lines() -> Option<usize> {
    std::env::var("NEWT_SPILL_LINES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
}

/// Install (or release, with `None`) the session spill-detail override. The
/// one writer: `/spill`, `/detail` and `/settings detail` all land here.
pub fn set_session_spill_lines(rows: Option<usize>) {
    match rows {
        Some(n) => crate::process_env::set_var("NEWT_SPILL_LINES", &n.to_string()),
        None => crate::process_env::remove_var("NEWT_SPILL_LINES"),
    }
}

/// How a thinking model's streamed reasoning is surfaced — the `[tui] thinking`
/// key. Newt strips `<think>…</think>` from the reply regardless (#385); this
/// only controls the live human display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Cargo-style: reasoning streams as dim scrolled lines (kept in
    /// scrollback) with an ephemeral spinner line pinned at the bottom. On a
    /// TTY only; a pipe / `newt worker` shows nothing.
    ///
    /// Unbounded: a model that reasons for four hundred lines commits four
    /// hundred lines, which is what [`Self::Fold`] exists to stop.
    Stream,
    /// The default. Like [`Self::Stream`], but BOUNDED: the first
    /// `[tui] spill_lines` rows of reasoning commit as before, the rest are
    /// retained instead of printed, and the block closes with one line naming
    /// how long the model thought and how much is behind the fold.
    ///
    /// The same treatment tool output already gets, applied to reasoning —
    /// same budget, same `Fold` vocabulary, same `/spill open <id>` recovery —
    /// because "a wall of grey that buries the conversation spine" is the same
    /// problem whichever side produced it.
    #[default]
    Fold,
    /// No reasoning display at all (the answer still streams normally).
    Off,
}

pub(super) fn default_spill_lines() -> usize {
    3
}

pub(super) fn default_time_marker_secs() -> u64 {
    300
}

pub(super) fn default_tool_output_lines() -> usize {
    20
}

/// Key binding style for the chat REPL input line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    /// Readline / emacs-style bindings.
    Emacs,
    /// Vi / vim-style bindings — Esc for normal mode, i for insert.
    Vi,
    /// Nano-style: modeless, emacs-like bindings (the **default** — the most
    /// broadly approachable). Behaves like `Emacs` on the lean surface; it is a
    /// distinct, selectable label, and the rich-tui surface shows the nano `^G`
    /// help hint for it.
    #[default]
    Nano,
}

impl EditMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
            Self::Nano => "nano",
        }
    }

    /// Cycle through the modes (used by a single-key toggle): emacs → vi →
    /// nano → emacs.
    pub fn toggle(&self) -> Self {
        match self {
            Self::Emacs => Self::Vi,
            Self::Vi => Self::Nano,
            Self::Nano => Self::Emacs,
        }
    }
}

/// Chat REPL display density.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChatStyle {
    /// Just the caret symbol — no "newt" / "you" labels.
    #[default]
    Compact,
    /// Full "newt ▸" / "you $" labels before each message.
    Verbose,
}

impl ChatStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Verbose => "verbose",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::Compact => Self::Verbose,
            Self::Verbose => Self::Compact,
        }
    }
}
