//! Categorized help data for slash commands.
//! Used by `help_display()` in lib.rs to render grouped output.

use std::fmt::Write as FmtWrite;

/// A group of related commands with a header and lines.
#[derive(Clone, Debug)]
pub struct HelpSection {
    pub title: &'static str,
    /// Lines for this section (no leading slash prefix).
    pub lines: &'static [&'static str],
}

// ── Main command sections ────────────────────────────────────────

/// Core conversation commands.
pub static SECTION_MAIN: &[&str] = &[
    "/help - Show all available commands",
    "/new - Start a new conversation (alias: /reset, /restart)",
    "/end - End the current conversation (alias: /done)",
    "/remember <text> - Save a note to persistent memory (NOTES.md)",
    "/forget <keyword> - Remove a saved note",
    "/notes - List all saved notes",
];

// ── Model sections ───────────────────────────────────────────────

/// Backend and model configuration.
pub static SECTION_MODEL: &[&str] = &[
    "/backend [name] - Set or show the active backend (alias: /be, /model)",
    "/backends - List all configured backends",
    "/probe <model> - Test a model with a simple prompt and show latency/quality",
];

// ── Context sections ─────────────────────────────────────────────

/// Conversation context management.
pub static SECTION_CONTEXT: &[&str] = &[
    "/context - Show current conversation state (token count, messages)",
    "/compress - Compress the conversation to reduce token usage",
    "/restore <id> - Restore a previous conversation from session history",
    "/save - Save the current conversation to disk",
];

// ── Tool sections ────────────────────────────────────────────────

/// External tool integration.
pub static SECTION_TOOLS: &[&str] = &[
    "/mcp [server] [tool] [args...] - Run an MCP server tool (alias: /mcptool)",
    "/acp - Start the ACP worker for cross-agent communication",
];

// ── Permissions sections ─────────────────────────────────────────

/// Permission and access control.
pub static SECTION_PERMISSIONS: &[&str] = &[
    "/permissions [grant|revoke|list] <capability> <target> - Manage permissions (alias: /perm)",
    "/allow <command> - Allow a specific command permanently",
];

// ── Settings sections ────────────────────────────────────────────

/// Configuration and settings.
pub static SECTION_SETTINGS: &[&str] = &[
    "/config [key] [value] - Show or set configuration (alias: /set)",
    "/reset-config - Reset all configuration to defaults",
    "/log-level <level> - Set logging level (debug, info, warn, error)",
];

// ── Agent sections ───────────────────────────────────────────────

/// Agent behavior and execution.
pub static SECTION_AGENT: &[&str] = &[
    "/agent [name|list] - Switch to a specific agent persona or list available agents",
    "/plan [text] - Show the current task plan (alias: /todo)",
    "/stop - Stop the current operation/execution",
];

// ── Prompt sections ──────────────────────────────────────────────

/// Prompt and token management.
pub static SECTION_PROMPT: &[&str] = &[
    "/prompt [tokens|context|model] - Show prompt-related information (alias: /pt)",
    "/token-usage - Display current token usage statistics",
];

// ── Evaluation sections ──────────────────────────────────────────

/// Testing and evaluation.
pub static SECTION_EVAL: &[&str] = &[
    "/eval [suite|test] - Run an evaluation suite or specific test (alias: /e)",
    "/benchmark [suite] - Run a benchmark suite",
];

// ── System sections ──────────────────────────────────────────────

/// System and debugging.
pub static SECTION_SYSTEM: &[&str] = &[
    "/status - Show system status (version, backends, uptime)",
    "/debug [feature] - Enable debug mode for a specific feature",
    "/info - Show detailed system information",
];

// ── Help sections ────────────────────────────────────────────────

/// Help and documentation.
pub static SECTION_HELP: &[&str] = &[
    "/help <command> - Get help for a specific command (alias: /h)",
    "/docs [topic] - Open documentation in browser",
];

// ── Builder ──────────────────────────────────────────────────────

/// Build all sections in display order.
pub fn build_sections() -> Vec<HelpSection> {
    vec![
        HelpSection { title: "Main Commands", lines: SECTION_MAIN },
        HelpSection { title: "Model & Backend", lines: SECTION_MODEL },
        HelpSection { title: "Context Management", lines: SECTION_CONTEXT },
        HelpSection { title: "Tools & Integration", lines: SECTION_TOOLS },
        HelpSection { title: "Permissions", lines: SECTION_PERMISSIONS },
        HelpSection { title: "Settings", lines: SECTION_SETTINGS },
        HelpSection { title: "Agent", lines: SECTION_AGENT },
        HelpSection { title: "Prompt & Tokens", lines: SECTION_PROMPT },
        HelpSection { title: "Evaluation", lines: SECTION_EVAL },
        HelpSection { title: "System", lines: SECTION_SYSTEM },
        HelpSection { title: "Help", lines: SECTION_HELP },
    ]
}

/// Render all sections to a single formatted string.
pub fn format_help() -> String {
    let mut out = String::new();
    let sections = build_sections();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "## {}", section.title);
        for line in section.lines {
            let _ = writeln!(out, "  {}", line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_help_contains_main_section() {
        let help = format_help();
        assert!(help.contains("## Main Commands"));
        assert!(help.contains("/help - Show all available commands"));
        assert!(help.contains("/new - Start a new conversation"));
    }

    #[test]
    fn test_format_help_contains_all_sections() {
        let help = format_help();
        assert!(help.contains("## Model & Backend"));
        assert!(help.contains("## Context Management"));
        assert!(help.contains("## Tools & Integration"));
        assert!(help.contains("## Permissions"));
        assert!(help.contains("## Settings"));
        assert!(help.contains("## Agent"));
        assert!(help.contains("## Prompt & Tokens"));
        assert!(help.contains("## Evaluation"));
        assert!(help.contains("## System"));
        assert!(help.contains("## Help"));
    }

    #[test]
    fn test_format_help_contains_all_commands() {
        let help = format_help();
        // Core commands
        assert!(help.contains("/remember <text>"));
        assert!(help.contains("/forget <keyword>"));
        assert!(help.contains("/notes"));
        // Model commands
        assert!(help.contains("/backend [name]"));
        assert!(help.contains("/backends"));
        assert!(help.contains("/probe <model>"));
        // Context commands
        assert!(help.contains("/context"));
        assert!(help.contains("/compress"));
        assert!(help.contains("/restore <id>"));
        assert!(help.contains("/save"));
        // Tool commands
        assert!(help.contains("/mcp [server]"));
        assert!(help.contains("/acp"));
        // Permission commands
        assert!(help.contains("/permissions [grant|revoke|list]"));
        assert!(help.contains("/allow <command>"));
        // Settings commands
        assert!(help.contains("/config [key]"));
        assert!(help.contains("/reset-config"));
        assert!(help.contains("/log-level <level>"));
        // Agent commands
        assert!(help.contains("/agent [name|list]"));
        assert!(help.contains("/plan [text]"));
        assert!(help.contains("/stop"));
        // Prompt commands
        assert!(help.contains("/prompt [tokens|context|model]"));
        assert!(help.contains("/token-usage"));
        // Eval commands
        assert!(help.contains("/eval [suite|test]"));
        assert!(help.contains("/benchmark [suite]"));
        // System commands
        assert!(help.contains("/status"));
        assert!(help.contains("/debug [feature]"));
        assert!(help.contains("/info"));
    }

    #[test]
    fn test_format_help_no_empty_sections() {
        let sections = build_sections();
        for section in &sections {
            assert!(!section.lines.is_empty(), "Section '{}' has no lines", section.title);
        }
    }

    #[test]
    fn test_build_sections_order() {
        let sections = build_sections();
        assert_eq!(sections[0].title, "Main Commands");
        assert_eq!(sections[1].title, "Model & Backend");
        assert_eq!(sections[2].title, "Context Management");
    }

    #[test]
    fn test_format_help_no_leading_blank_line() {
        let help = format_help();
        assert!(!help.starts_with('\n'), "Help output should not start with a blank line");
    }

    #[test]
    fn test_format_help_trailing_newline() {
        let help = format_help();
        // The last section's last line ends with \n from writeln!
        assert!(help.ends_with('\n'), "Help output should end with newline");
    }
}
