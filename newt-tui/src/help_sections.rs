//! Categorized help data for slash commands.
//! Used by `help_display()` in lib.rs to render grouped output.
//!
//! Rollups let the user drill down: a top-level command shows its group's
//! overview (which sub-commands exist), and `/cmd help <name>` resolves to
//! one of three levels — rollup page, detail page, or one-shot `command_help_page`.

use std::fmt::Write as FmtWrite;

/// A group of related commands with a header and lines.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct HelpSection {
    pub title: &'static str,
    /// Lines for this section (no leading slash prefix).
    pub lines: &'static [&'static str],
}

// ── Rollup infrastructure ────────────────────────────────────────

/// A page shown when the user drills into a command group. Contains the
/// group's sub-commands plus optional nested detail pages for each one.
#[derive(Clone, Debug)]
pub struct RollupPage {
    /// Display title (e.g. "Conversation", "Models").
    pub title: &'static str,
    /// One-line summary of what this group does.
    pub summary: &'static str,
    /// Sub-commands with their one-line descriptions and optional detail key.
    /// `detail_key` is the command name to look up in `command_detail()`.
    pub entries: &'static [RollupEntry],
}

/// A single entry inside a rollup page.
#[derive(Clone, Debug)]
pub struct RollupEntry {
    /// The slash command (without leading `/`).
    pub cmd: &'static str,
    /// Short description shown inline.
    pub desc: &'static str,
    /// Optional detail key — if set, this entry has a drill-down page.
    pub detail_key: Option<&'static str>,
}

// ── Rollup data ────────────────────────────────────────────────

/// Conversation commands rollup (main + context).
pub static ROLLUP_CONVERSATION: RollupPage = RollupPage {
    title: "Conversation",
    summary: "Start, end, and manage your chat session.",
    entries: &[
        RollupEntry {
            cmd: "help",
            desc: "Show all available commands",
            detail_key: None,
        },
        RollupEntry {
            cmd: "new",
            desc: "Start a new conversation (alias: /reset, /restart)",
            detail_key: None,
        },
        RollupEntry {
            cmd: "end",
            desc: "End the current conversation (alias: /done)",
            detail_key: None,
        },
        RollupEntry {
            cmd: "remember",
            desc: "Save a note to persistent memory",
            detail_key: Some("memory"),
        },
        RollupEntry {
            cmd: "forget",
            desc: "Remove a saved note",
            detail_key: None,
        },
        RollupEntry {
            cmd: "notes",
            desc: "List all saved notes",
            detail_key: None,
        },
        RollupEntry {
            cmd: "context",
            desc: "Show current conversation state",
            detail_key: None,
        },
        RollupEntry {
            cmd: "compress",
            desc: "Compress the conversation to reduce token usage",
            detail_key: Some("compress"),
        },
        RollupEntry {
            cmd: "restore",
            desc: "Restore a previous conversation from session history",
            detail_key: None,
        },
        RollupEntry {
            cmd: "save",
            desc: "Save the current conversation to disk",
            detail_key: None,
        },
    ],
};

/// Model and backend commands rollup.
pub static ROLLUP_MODELS: RollupPage = RollupPage {
    title: "Models & Backends",
    summary: "Inspect, switch, and classify models.",
    entries: &[
        RollupEntry {
            cmd: "models",
            desc: "List models on the active endpoint",
            detail_key: Some("models"),
        },
        RollupEntry {
            cmd: "model",
            desc: "Switch the model on the active backend",
            detail_key: Some("model"),
        },
        RollupEntry {
            cmd: "backend",
            desc: "Switch the backend wire protocol",
            detail_key: Some("backend"),
        },
        RollupEntry {
            cmd: "backends",
            desc: "List configured backends, or switch to one by name",
            detail_key: Some("backends"),
        },
        RollupEntry {
            cmd: "probe",
            desc: "Test a model with a simple prompt and show latency/quality",
            detail_key: Some("probe"),
        },
    ],
};

/// Tools and integration rollup.
pub static ROLLUP_TOOLS: RollupPage = RollupPage {
    title: "Tools & Integration",
    summary: "External tool integration and agent communication.",
    entries: &[
        RollupEntry {
            cmd: "mcp",
            desc: "Run an MCP server tool (alias: /mcptool)",
            detail_key: None,
        },
        RollupEntry {
            cmd: "acp",
            desc: "Start the ACP worker for cross-agent communication",
            detail_key: None,
        },
        RollupEntry {
            cmd: "tools",
            desc: "Manage available tools",
            detail_key: Some("tools"),
        },
    ],
};

/// Settings and configuration rollup.
pub static ROLLUP_SETTINGS: RollupPage = RollupPage {
    title: "Settings & Configuration",
    summary: "Configuration, permissions, and system settings.",
    entries: &[
        RollupEntry {
            cmd: "config",
            desc: "Show or set configuration (alias: /set)",
            detail_key: Some("config"),
        },
        RollupEntry {
            cmd: "reset-config",
            desc: "Reset all configuration to defaults",
            detail_key: None,
        },
        RollupEntry {
            cmd: "log-level",
            desc: "Set logging level (debug, info, warn, error)",
            detail_key: None,
        },
        RollupEntry {
            cmd: "permissions",
            desc: "Manage permissions (alias: /perm)",
            detail_key: Some("permissions"),
        },
        RollupEntry {
            cmd: "allow",
            desc: "Allow a specific command permanently",
            detail_key: None,
        },
    ],
};

/// Agent and evaluation rollup.
pub static ROLLUP_AGENT: RollupPage = RollupPage {
    title: "Agent & Evaluation",
    summary: "Agent behavior, personas, testing, and system info.",
    entries: &[
        RollupEntry {
            cmd: "agent",
            desc: "Switch to a specific agent persona or list available agents",
            detail_key: Some("agent"),
        },
        RollupEntry {
            cmd: "plan",
            desc: "Show the current task plan (alias: /todo)",
            detail_key: Some("plan"),
        },
        RollupEntry {
            cmd: "stop",
            desc: "Stop the current operation/execution",
            detail_key: None,
        },
        RollupEntry {
            cmd: "persona",
            desc: "Manage personas",
            detail_key: Some("persona"),
        },
        RollupEntry {
            cmd: "eval",
            desc: "Run an evaluation suite or specific test (alias: /e)",
            detail_key: Some("eval"),
        },
        RollupEntry {
            cmd: "benchmark",
            desc: "Run a benchmark suite",
            detail_key: None,
        },
        RollupEntry {
            cmd: "status",
            desc: "Show system status (version, backends, uptime)",
            detail_key: None,
        },
        RollupEntry {
            cmd: "debug",
            desc: "Enable debug mode for a specific feature",
            detail_key: None,
        },
        RollupEntry {
            cmd: "info",
            desc: "Show detailed system information",
            detail_key: None,
        },
    ],
};

/// DGX hub commands rollup.
pub static ROLLUP_DGX: RollupPage = RollupPage {
    title: "DGX Hub",
    summary: "Pull, list, and inspect models from the DGX hub.",
    entries: &[RollupEntry {
        cmd: "dgx",
        desc: "Manage DGX hub models",
        detail_key: Some("dgx"),
    }],
};

/// Conversation management rollup (separate from main conversation).
pub static ROLLUP_CONVERSATION_MGMT: RollupPage = RollupPage {
    title: "Conversation Management",
    summary: "Advanced conversation lifecycle operations.",
    entries: &[RollupEntry {
        cmd: "conversation",
        desc: "Manage conversation history and export",
        detail_key: Some("conversation"),
    }],
};

/// All rollup pages, keyed by the top-level command name.
pub static SECTION_MAIN_ROLLUPS: &[(&str, &RollupPage)] = &[
    ("conversation", &ROLLUP_CONVERSATION),
    ("models", &ROLLUP_MODELS),
    ("tools", &ROLLUP_TOOLS),
    ("settings", &ROLLUP_SETTINGS),
    ("agent", &ROLLUP_AGENT),
    ("dgx", &ROLLUP_DGX),
    ("conversation-mgmt", &ROLLUP_CONVERSATION_MGMT),
];

// Helper: check if a command has rollup pages.
#[allow(dead_code)]
pub fn topic_has_rollups(topic: &str) -> bool {
    SECTION_MAIN_ROLLUPS.iter().any(|(name, _)| *name == topic)
}

/// Get the rollup page for a topic (if any).
#[allow(dead_code)]
pub fn rollup_page_for_topic(topic: &str) -> Option<&'static RollupPage> {
    SECTION_MAIN_ROLLUPS
        .iter()
        .find(|(name, _)| *name == topic)
        .map(|(_, page)| *page)
}

/// Get the detail lines for a command (if any).
#[allow(dead_code)]
pub fn rollup_detail_for(cmd: &str) -> Option<&'static [&'static str]> {
    command_detail(cmd)
}

// NOTE: SECTION_MAIN data lives in RollupPage::ROLLUP_CONVERSATION.entries.
// ── Model sections (rendered via ROLLUP_MODELS) ────────────────
// NOTE: SECTION_MODEL data lives in RollupPage::ROLLUP_MODELS.entries.

// ── Prompt sections ──────────────────────────────────────────────
// NOTE: Prompt sections removed — their data now lives in ROLLUP_CONVERSATION.entries.
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
#[allow(dead_code)]
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

/// Remaining static sections (no rollup covers them). Kept in display order.
// NOTE: REMAINING_SECTIONS removed — format_help_for_topic() now uses rollup pages directly.
/// Render all help — rollups first (summary + drill-down hint), then remaining static sections.
pub fn format_help() -> String {
    let mut out = String::new();

    // Rollup pages in canonical order.
    for (i, (_name, page)) in SECTION_MAIN_ROLLUPS.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let summary = format_rollup_summary(page);
        for line in &summary {
            let _ = writeln!(out, "{line}");
        }
        // Drill-down hint when the rollup has more detail available.
        if page.entries.len() > 1 {
            let topic_name = SECTION_MAIN_ROLLUPS[i].0;
            let _ = writeln!(
                out,
                "\n  ... and {} more — use /cmd help {} for details",
                page.entries.len() - 1,
                topic_name
            );
        } else if page.summary.is_empty() {
            // No summary text → show the single entry as-is.
            let _ = writeln!(out);
            for line in page.entries[0].desc.lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
    }

    // Remaining small sections (no rollup).
    {
        let prompt_lines: &[&str] = &[
            "/prompt [tokens|context|model] - Show prompt-related information",
            "/token-usage - Display current token usage statistics",
        ];
        let _ = writeln!(out);
        let _ = writeln!(out, "\n## Prompt & Tokens");
        for line in prompt_lines {
            let _ = writeln!(out, "  {line}");
        }

        let help_lines: &[&str] = &[
            "/help - Show this help message",
            "/version - Display version info",
        ];
        let _ = writeln!(out);
        let _ = writeln!(out, "\n## Help & Docs");
        for line in help_lines {
            let _ = writeln!(out, "  {line}");
        }
    }

    out
}

/// Format a rollup page as summary rows (one per entry).
fn format_rollup_summary(page: &RollupPage) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in page.entries {
        let desc = if entry.desc.is_empty() {
            "No description available"
        } else {
            entry.desc
        };
        // Truncate long descriptions to fit display width (80 chars total)
        let max_desc_len = 65;
        let display_desc = if desc.len() > max_desc_len {
            format!("{}...", &desc[..max_desc_len - 3])
        } else {
            desc.to_string()
        };
        lines.push(format!("{:<12} {}", entry.cmd, display_desc));
    }
    lines
}

/// Format a rollup page with detail columns (one per entry, showing drill-down key).
pub fn format_rollup_detail(page: &RollupPage) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in page.entries {
        let desc = if entry.desc.is_empty() {
            "No description available"
        } else {
            entry.desc
        };
        // Truncate long descriptions to fit display width (80 chars total)
        let max_desc_len = 65;
        let display_desc = if desc.len() > max_desc_len {
            format!("{}...", &desc[..max_desc_len - 3])
        } else {
            desc.to_string()
        };
        // Show detail key in parentheses if present (indicates drill-down)
        let detail_hint = entry
            .detail_key
            .map(|k| format!(" (drill: /{k})"))
            .unwrap_or_default();
        lines.push(format!("{:<12} {}{}", entry.cmd, display_desc, detail_hint));
    }
    lines
}

/// Format help for a specific topic — either a rollup page or a one-shot
/// detail page. Returns an empty string when nothing matches so the caller can
/// render "no help available" instead of crashing with a missing-constant error.
pub fn format_help_for_topic(topic: &str) -> String {
    // 1. Rollup lookup — progressive dispatch target (conversation, models, tools, settings, agent).
    if let Some(page) = rollup_page_for_topic(topic) {
        return format_rollup_detail(page).join("\n");
    }
    // 2. One-shot detail page for /cmd help drill-downs (dgx, conversation, persona, model, config, permissions, tools, agent, memory, eval, plan).
    if let Some(lines) = command_detail(topic) {
        return lines.join("\n");
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_help_contains_main_section() {
        let help = format_help();
        assert!(help.contains("## Main Commands"));
        // Rollup-based: should mention /cmd help for details
        assert!(help.contains("/cmd help"), "Should include drill-down hint");
    }

    #[test]
    fn test_format_help_contains_all_sections() {
        let help = format_help();
        // Sections are now rollup pages; check that key topics appear
        assert!(help.contains("Conversation"));
        assert!(help.contains("Model & Backend"));
        assert!(help.contains("Context Management"));
        assert!(help.contains("Tools & Integration"));
        assert!(help.contains("Permissions"));
        assert!(help.contains("Settings"));
        assert!(help.contains("Agent"));
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

    #[test]
    fn test_rollup_page_for_conversation_topic() {
        let page = rollup_page_for_topic(ROLLUP_CONVERSATION.title);
        assert!(
            page.is_some(),
            "Conversation topic should have a rollup page"
        );
        let page = page.unwrap();
        assert_eq!(page.title, ROLLUP_CONVERSATION.title);
        // Should have entries for each command in the rollup
        assert!(
            !page.entries.is_empty(),
            "Conversation rollup should have entries"
        );
    }

    #[test]
    fn test_rollup_page_for_models_topic() {
        let page = rollup_page_for_topic(ROLLUP_MODELS.title);
        assert!(page.is_some(), "Models topic should have a rollup page");
        let page = page.unwrap();
        assert_eq!(page.title, ROLLUP_MODELS.title);
        assert!(
            !page.entries.is_empty(),
            "Models rollup should have entries"
        );
    }

    #[test]
    fn test_rollup_page_for_tools_topic() {
        let page = rollup_page_for_topic(ROLLUP_TOOLS.title);
        assert!(page.is_some(), "Tools topic should have a rollup page");
        let page = page.unwrap();
        assert_eq!(page.title, ROLLUP_TOOLS.title);
        assert!(!page.entries.is_empty(), "Tools rollup should have entries");
    }

    #[test]
    fn test_rollup_page_for_settings_topic() {
        let page = rollup_page_for_topic(ROLLUP_SETTINGS.title);
        assert!(page.is_some(), "Settings topic should have a rollup page");
        let page = page.unwrap();
        assert_eq!(page.title, ROLLUP_SETTINGS.title);
        assert!(
            !page.entries.is_empty(),
            "Settings rollup should have entries"
        );
    }

    #[test]
    fn test_format_help_uses_rollups_when_available() {
        let help = format_help();
        // When a topic has rollups, format_help should show summary rows
        // and mention /cmd help for detailed viewing
        assert!(
            help.contains("/cmd help"),
            "Help with rollups should mention /cmd help for details"
        );
    }

    #[test]
    fn test_topic_has_rollups_conversation() {
        assert!(topic_has_rollups("conversation"));
        assert!(topic_has_rollups("conv"));
    }

    #[test]
    fn test_topic_has_rollups_models() {
        assert!(topic_has_rollups("models"));
        assert!(topic_has_rollups("model"));
    }

    #[test]
    fn test_topic_has_rollups_tools() {
        assert!(topic_has_rollups("tools"));
        assert!(topic_has_rollups("tool"));
    }

    #[test]
    fn test_topic_has_rollups_settings() {
        assert!(topic_has_rollups("settings"));
        assert!(topic_has_rollups("config"));
    }

    #[test]
    fn test_topic_no_rollups_unknown() {
        assert!(!topic_has_rollups("unknown-topic"));
        assert!(!topic_has_rollups("nonexistent"));
    }

    #[test]
    fn test_format_help_conversation_uses_rollup() {
        let help = format_help_for_topic("conversation");
        // Should show summary rows from rollup
        assert!(
            help.contains("/new"),
            "Should mention /new command in conversation rollup"
        );
        assert!(
            help.contains("/resume"),
            "Should mention /resume command in conversation rollup"
        );
    }

    #[test]
    fn test_format_help_models_uses_rollup() {
        let help = format_help_for_topic("models");
        // Should show summary rows from rollup
        assert!(
            help.contains("/model set"),
            "Should mention /model set command in models rollup"
        );
        assert!(
            help.contains("/provider list"),
            "Should mention /provider list command in models rollup"
        );
    }

    #[test]
    fn test_format_help_tools_uses_rollup() {
        let help = format_help_for_topic("tools");
        // Should show summary rows from rollup
        assert!(
            help.contains("/read"),
            "Should mention /read command in tools rollup"
        );
        assert!(
            help.contains("/write"),
            "Should mention /write command in tools rollup"
        );
    }

    #[test]
    fn test_format_help_settings_uses_rollup() {
        let help = format_help_for_topic("settings");
        // Should show summary rows from rollup
        assert!(
            help.contains("/config get"),
            "Should mention /config get command in settings rollup"
        );
        assert!(
            help.contains("/reset"),
            "Should mention /reset command in settings rollup"
        );
    }

    #[test]
    fn test_rollup_page_summary_row_format() {
        let page = rollup_page_for_topic(ROLLUP_CONVERSATION.title);
        // Each entry should have a command and description
        for (i, entry) in page.unwrap().entries.iter().enumerate() {
            assert!(!entry.cmd.is_empty(), "Entry {i} cmd should not be empty");
            assert!(!entry.desc.is_empty(), "Entry {i} desc should not be empty");
        }
    }

    #[test]
    fn test_rollup_page_has_entries_not_footer() {
        let page = rollup_page_for_topic(ROLLUP_CONVERSATION.title);
        // RollupPage has entries, no footer field — format_help renders a hint per entry with detail_key
        assert!(
            !page.unwrap().entries.is_empty(),
            "Conversation rollup should have entries"
        );
    }

    #[test]
    fn test_format_help_case_insensitive() {
        let help1 = format_command_help("CONVERSATION");
        let help2 = format_command_help("conversation");
        // Both should work (case-insensitive matching)
        assert!(
            help1.unwrap().is_empty(),
            "Should handle uppercase topic name"
        );
        assert!(
            help2.unwrap().is_empty(),
            "Should handle lowercase topic name"
        );
    }

    #[test]
    fn test_format_help_for_topic_empty_input() {
        let help = format_help_for_topic("");
        // Empty topic name should return empty string or error message
        assert!(help.is_empty(), "Empty topic should produce no output");
    }
}
