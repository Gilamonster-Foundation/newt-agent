//! Newt-Agent TUI surfaces.
//!
//! - `run_code`     — chat REPL with ANSI splash (feat/tui-splash-screen)
//! - `run_settings` — interactive settings TUI (this branch)
//! - `run_pilot`    — drake-swarm dashboard (stub)

mod settings;

pub use settings::run_settings;

pub fn run_code(_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_code not yet implemented — see feat/tui-splash-screen")
}

pub fn run_pilot(_flight_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_pilot not yet implemented")
}
