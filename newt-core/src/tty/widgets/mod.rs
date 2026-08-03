//! **The widget suite** — values that render to strings, routed through the
//! arbiter's four primitives and nothing else.
//!
//! Governed by `docs/decisions/tty_widget_suite.md`, which is itself
//! subordinate to `docs/decisions/plain_scroller_tui.md`. The hard boundary,
//! restated here because this is where it will be tested:
//!
//! - Every widget is a **pure formatter returning `String` / `Vec<String>`**,
//!   plus a thin emitter that routes through [`LineLease::emit_line`],
//!   [`Terminal::emit_line`], or [`PromptWindow`].
//! - **No `MoveTo`, no absolute row addressing, no alternate screen, no redraw
//!   method on a rendered value, no layout engine, no ratatui.** A value that
//!   renders once and is printed is a formatter; a value that can repaint
//!   itself with a highlighted row needs raw mode and *is* the violation. The
//!   word "widget" is the trap in the issue's own framing.
//! - The suite is **unconditional** — no `markdown` / `rich-tui` / `live-spill`
//!   feature dependency — so the headless wyvern strip carries it.
//!
//! Placement is enforcement: `ratatui` is absent from `newt-core/Cargo.toml`
//! entirely, so a suite living here *cannot* grow a full-screen surface, where
//! a sibling crate could have quietly added the dependency and still passed CI.
//!
//! [`LineLease::emit_line`]: crate::tty::LineLease::emit_line
//! [`Terminal::emit_line`]: crate::tty::Terminal::emit_line
//! [`PromptWindow`]: crate::tty::PromptWindow

pub mod notice;
pub mod question;

pub use notice::{Level, Notice};
pub use question::{Action, Question};
