//! Newt-Agent TUI — two screens, ratatui-driven.
//!
//! - `code` mode: chat / file pane / diff preview / apply-or-reject.
//! - `pilot` mode: drake-swarm dashboard (per-rung status, scorecards).
//!
//! v0 stubs only — surfaces wired in v0.2 / v0.4.

pub fn run_code(_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_code not yet implemented")
}

pub fn run_pilot(_flight_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_pilot not yet implemented")
}
