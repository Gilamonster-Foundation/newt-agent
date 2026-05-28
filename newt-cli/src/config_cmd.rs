//! `newt config` — print the resolved configuration.

use newt_core::Config;
use std::path::Path;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };
    let toml_str = toml::to_string_pretty(&config)?;
    println!("# Resolved Newt configuration\n# Source: Config::resolve() search order\n");
    println!("{toml_str}");
    Ok(())
}
