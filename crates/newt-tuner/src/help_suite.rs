use anyhow::Result;
use clap::{Command, CommandFactory, FromArgMatches};

#[derive(Debug, Clone, Copy)]
pub struct HelpSuite;

impl HelpSuite {
    /// Apply newline-separated option help rendering to this command and all subcommands.
    #[inline]
    fn apply_to(command: &mut Command) {
        *command = command.clone().next_line_help(true);
        for child in command.get_subcommands_mut() {
            Self::apply_to(child);
        }
    }

    /// Build a parser command tree with shared help style enabled.
    pub fn command<T: CommandFactory>() -> Command {
        let mut command = T::command();
        Self::apply_to(&mut command);
        command
    }

    /// Parse process args using the shared help style.
    pub fn parse_with_help<T: CommandFactory + FromArgMatches>() -> Result<T> {
        let cmd = Self::command::<T>();
        let matches = cmd.get_matches();
        T::from_arg_matches(&matches).map_err(anyhow::Error::new)
    }
}

/// Build a parser command tree with shared help style enabled.
#[allow(dead_code)]
pub fn help_command<T: CommandFactory>() -> Command {
    HelpSuite::command::<T>()
}

/// Parse process args using the shared help style.
pub fn parse_with_help<T: CommandFactory + FromArgMatches>() -> Result<T> {
    HelpSuite::parse_with_help::<T>()
}
