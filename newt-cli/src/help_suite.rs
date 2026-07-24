//! Shared, reusable help formatting for all clap help surfaces.
//!
//! The CLI has many nested command trees. A single helper here ensures that any
//! command whose help text is rendered uses the same wrapping + indentation
//! behavior.

use anyhow::Result;
use clap::{Command, CommandFactory, FromArgMatches};

/// Reusable help behavior shared by all CLI help entrypoints.
#[derive(Debug, Clone, Copy)]
pub struct HelpSuite;

impl HelpSuite {
    /// Apply the project's preferred help formatting style to this command and all
    /// nested subcommands.
    #[inline]
    fn apply_to(command: &mut Command) {
        *command = command.clone().next_line_help(true);
        for child in command.get_subcommands_mut() {
            Self::apply_to(child);
        }
    }

    /// Build a parser command tree with the shared help format applied.
    pub fn command<T: CommandFactory>() -> Command {
        let mut command = T::command();
        Self::apply_to(&mut command);
        command
    }

    /// Parse the active process args with the shared help format applied.
    pub fn parse_with_help<T: CommandFactory + FromArgMatches>() -> Result<T> {
        let cmd = Self::command::<T>();
        let matches = cmd.get_matches();
        T::from_arg_matches(&matches).map_err(anyhow::Error::new)
    }
}

/// Construct the `newt` parser command with shared help formatting applied.
pub fn help_command() -> Command {
    HelpSuite::command::<crate::Cli>()
}

/// Parse `newt` args with shared help formatting applied.
pub fn parse_with_help() -> Result<crate::Cli> {
    HelpSuite::parse_with_help::<crate::Cli>()
}
