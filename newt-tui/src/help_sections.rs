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

// ── Per-command detail pages (for `/cmd help`) ───────────────────

/// Detailed help for the `/dgx` command family.
pub static HELP_DGX: &[&str] = &[
    "/dgx pull <model> - Pull a model from DGX hub",
    "/dgx list [query]  - List available models on DGX hub",
    "/dgx inspect <id>  - Show full metadata for a model",
    "/dgx verify <path> - Verify integrity of a downloaded model",
];

/// Detailed help for the `/conversation` command family.
pub static HELP_CONVERSATION: &[&str] = &[
    "/conversation history - Show conversation history",
    "/conversation save [name] - Save current conversation with optional name",
    "/conversation restore <id> - Restore a saved conversation",
    "/conversation export <format> - Export conversation (json, md, txt)",
    "/conversation truncate [n] - Keep last n messages",
];

/// Detailed help for the `/persona` command family.
pub static HELP_PERSONA: &[&str] = &[
    "/persona list - List available personas",
    "/persona show <name> - Show persona details",
    "/persona use <name> - Switch to a different persona",
    "/persona create [file] - Create a new persona from template or file",
    "/persona delete <name> - Remove a persona",
];

/// Detailed help for the `/model` command family.
pub static HELP_MODEL: &[&str] = &[
    "/model set <name> - Set active model (alias: /backend)",
    "/model list - List available models",
    "/model info [name] - Show model details and capabilities",
    "/model probe <model> - Quick quality test with latency",
];

/// Detailed help for the `/config` command family.
pub static HELP_CONFIG: &[&str] = &[
    "/config show [key] - Show config value(s)",
    "/config set <key> <value> - Set a config value",
    "/config reset - Reset all settings to defaults",
    "/config path - Show config file location",
];

/// Detailed help for the `/permissions` command family.
pub static HELP_PERMISSIONS: &[&str] = &[
    "/permissions list - List current permissions",
    "/permissions grant <cap> <target> - Grant a permission",
    "/permissions revoke <cap> - Revoke a permission",
];

/// Detailed help for the `/tools` command family.
pub static HELP_TOOLS: &[&str] = &[
    "/tools list - List available tools",
    "/tools enable <name> - Enable a tool",
    "/tools disable <name> - Disable a tool",
    "/tools info <name> - Show tool details and usage",
];

/// Detailed help for the `/agent` command family.
pub static HELP_AGENT: &[&str] = &[
    "/agent list - List available agents",
    "/agent use <name> - Switch to a different agent",
    "/agent info [name] - Show agent details and capabilities",
];

/// Detailed help for the `/memory` command family.
pub static HELP_MEMORY: &[&str] = &[
    "/memory show - List all saved notes (alias: /notes)",
    "/memory add <text> - Add a note (alias: /remember)",
    "/memory remove <keyword> - Remove a note by keyword",
    "/memory search <query> - Search notes for matching text",
];

/// Detailed help for the `/eval` command family.
pub static HELP_EVAL: &[&str] = &[
    "/eval list [suite] - List available eval suites or tests",
    "/eval run <suite|test> - Run an evaluation suite or test",
    "/eval report [id] - Show results for a past evaluation",
];

/// Detailed help for the `/plan` command family.
pub static HELP_PLAN: &[&str] = &[
    "/plan show - Show current task plan (alias: /todo)",
    "/plan add <step> - Add a step to the plan",
    "/plan done <n> - Mark step n as completed",
    "/plan clear - Clear the current plan",
];

// ── Map of command → detail page for `/cmd help` progressive disclosure ──

/// Returns the detailed help lines for a given slash command, if one exists.
pub fn command_detail(cmd: &str) -> Option<&'static [&'static str]> {
    match cmd {
        "dgx" => Some(HELP_DGX),
        "conversation" => Some(HELP_CONVERSATION),
        "persona" | "pers" => Some(HELP_PERSONA),
        "model" | "backend" => Some(HELP_MODEL),
        "config" | "set" => Some(HELP_CONFIG),
        "permissions" | "perm" => Some(HELP_PERMISSIONS),
        "tools" => Some(HELP_TOOLS),
        "agent" => Some(HELP_AGENT),
        "memory" | "remember" | "notes" => Some(HELP_MEMORY),
        "eval" | "e" => Some(HELP_EVAL),
        "plan" | "todo" => Some(HELP_PLAN),
        _ => None,
    }
}

/// Render a single command's detail page.
pub fn format_command_help(cmd: &str) -> Option<String> {
    let lines = command_detail(cmd)?;
    if lines.is_empty() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "## /{cmd} help");
    for line in lines {
        let _ = writeln!(out, "  {line}");
    }
    Some(out)
}

// ── Builder ──────────────────────────────────────────────────────

/// Build all sections in display order.
pub fn build_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Main Commands",
            lines: SECTION_MAIN,
        },
        HelpSection {
            title: "Model & Backend",
            lines: SECTION_MODEL,
        },
        HelpSection {
            title: "Context Management",
            lines: SECTION_CONTEXT,
        },
        HelpSection {
            title: "Tools & Integration",
            lines: SECTION_TOOLS,
        },
        HelpSection {
            title: "Permissions",
            lines: SECTION_PERMISSIONS,
        },
        HelpSection {
            title: "Settings",
            lines: SECTION_SETTINGS,
        },
        HelpSection {
            title: "Agent",
            lines: SECTION_AGENT,
        },
        HelpSection {
            title: "Prompt & Tokens",
            lines: SECTION_PROMPT,
        },
        HelpSection {
            title: "Evaluation",
            lines: SECTION_EVAL,
        },
        HelpSection {
            title: "System",
            lines: SECTION_SYSTEM,
        },
        HelpSection {
            title: "Help",
            lines: SECTION_HELP,
        },
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
            assert!(
                !section.lines.is_empty(),
                "Section '{}' has no lines",
                section.title
            );
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
        assert!(
            !help.starts_with('\n'),
            "Help output should not start with a blank line"
        );
    }

    #[test]
    fn test_format_help_trailing_newline() {
        let help = format_help();
        // The last section's last line ends with \n from writeln!
        assert!(help.ends_with('\n'), "Help output should end with newline");
    }
}
