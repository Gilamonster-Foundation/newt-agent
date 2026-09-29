//! Newt integration for the optional `agent-toolchain` embedded Git adapter.

pub use agent_toolchain::embedded_git::{check_git_read_scope, git_tool_definition, GitTool};
pub(crate) use agent_toolchain::embedded_git::{
    definition_for_read_scope, is_scoped_read_op, read_only_definition,
};
