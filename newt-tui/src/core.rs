//! Core TUI functionality for the newt agent system

// This module contains the core TUI components and structures

pub use crate::chat::{run_chat, ChatConfig, ChatError, ChatOutcome};
pub use crate::commands::{handle_command, CommandError, CommandOutcome};
pub use crate::permissions::{handle_permission_request, PermissionError, PermissionOutcome};
pub use crate::prompt::{handle_prompt, PromptError, PromptOutcome};
pub use crate::settings_form::{handle_settings_form, SettingsFormError, SettingsFormOutcome};
pub use crate::skills_form::{handle_skills_form, SkillsFormError, SkillsFormOutcome};
pub use crate::transcript_pager::{handle_transcript_pager, TranscriptPagerError, TranscriptPagerOutcome};
pub use crate::tab_switch::{handle_tab_switch, TabSwitchError, TabSwitchOutcome};
pub use crate::tabs::{handle_tabs, TabsError, TabsOutcome};
pub use crate::workspace_state::{handle_workspace_state, WorkspaceStateError, WorkspaceStateOutcome};
pub use crate::workspace_launch::{handle_workspace_launch, WorkspaceLaunchError, WorkspaceLaunchOutcome};

// Re-exports for the TUI core components
pub use crate::auth_command::run_auth;
pub use crate::brand::newt_logo;
pub use crate::color::colorize;
pub use crate::context_window::context_window_info;
pub use crate::crew_form::crew_form;
pub use crate::flock_intake::flock_intake;
pub use crate::help::help;
pub use crate::migration_notices::migration_notices;
pub use crate::navigator_cmds::navigator_cmds;
pub use crate::session_capability::session_capability;
pub use crate::workspace_settings::workspace_settings;
pub use crate::codex_env::{codex_env_allowed, codex_env_backend};
pub use crate::turn_input::{with_interrupt_watch, with_live_spill_watch};
pub use crate::roadmap_cmds::{autocapture_commit_after_turn, git_head_short, handle_roadmap_command, roadmap_subcommand_reads};
pub use crate::preference_pins::{apply_startup_preference_pin, restore_preference_pin, ConversationPreferenceSwitch, PinDegraded, PinRestore, PreferenceBaseline, StartupConversation};
pub use crate::persona_route::apply_persona_backend;
pub use crate::danger::{danger_tier, DangerTier};
pub use crate::event_receipt::{event_receipt, EventReceipt};
pub use crate::fold_list::fold_list;
pub use crate::inline_viewport::inline_viewport;
pub use crate::interaction_view::interaction_view;
pub use crate::clarification_modal::clarification_modal;
pub use crate::live_spill::live_spill;
pub use crate::prompt_components_form::prompt_components_form;
pub use crate::scrollbar::scrollbar;
pub use crate::shell::shell;
pub use crate::slash_registry::slash_registry;
pub use crate::spill_view::spill_view;
pub use crate::status_topics::status_topics;
pub use crate::system_prompt_edit::system_prompt_edit;
pub use crate::type_ahead::type_ahead;
pub use crate::mcp::{classify_readiness, readiness_label, saved_oauth_valid, McpReadiness};
pub use crate::mcp_manager::mcp_manager;
pub use crate::mcp_panel::mcp_panel;
pub use crate::mcp_token::mcp_token;
pub use crate::probe::probe;
pub use crate::terminal_hyperlink::terminal_hyperlink;
pub use crate::window_discovery::window_discovery;