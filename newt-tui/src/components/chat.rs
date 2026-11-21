//! Chat functionality for the TUI.

use std::sync::Arc;

use newt_core::mcp::McpTools;
use newt_core::prompt_intake::PromptIntake;
use newt_core::response::Response;
use newt_core::run_allowance::RunAllowance;
use newt_core::tool_search::ToolSearch;
use newt_core::transcript::Transcript;
use newt_core::tty::Tty;
use newt_core::untrusted::Untrusted;

use crate::turn_input::{with_interrupt_watch, with_live_spill_watch};

/// Chat context for the TUI.
pub struct ChatCtx {
    /// The backend for tool execution.
    pub tools: McpTools,
    /// The prompt intake system.
    pub prompt_intake: PromptIntake,
    /// The response system.
    pub response: Response,
    /// The run allowance.
    pub run_allowance: RunAllowance,
    /// The tool search system.
    pub tool_search: ToolSearch,
    /// The transcript.
    pub transcript: Transcript,
    /// The TTY.
    pub tty: Tty,
    /// The untrusted system.
    pub untrusted: Untrusted,
}

impl ChatCtx {
    /// Creates a new chat context.
    pub fn new(
        tools: McpTools,
        prompt_intake: PromptIntake,
        response: Response,
        run_allowance: RunAllowance,
        tool_search: ToolSearch,
        transcript: Transcript,
        tty: Tty,
        untrusted: Untrusted,
    ) -> Self {
        Self {
            tools,
            prompt_intake,
            response,
            run_allowance,
            tool_search,
            transcript,
            tty,
            untrusted,
        }
    }

    /// Runs the chat loop.
    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Implementation would go here
        unimplemented!()
    }
}

/// Chat command handler.
pub struct ChatCommand {
    /// The chat context.
    pub ctx: ChatCtx,
    /// The command name.
    pub name: String,
}

impl ChatCommand {
    /// Creates a new chat command.
    pub fn new(ctx: ChatCtx, name: String) -> Self {
        Self { ctx, name }
    }

    /// Executes the command.
    pub fn execute(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Implementation would go here
        unimplemented!()
    }
}