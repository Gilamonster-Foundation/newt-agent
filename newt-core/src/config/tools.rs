use serde::{Deserialize, Serialize};

/// Tool-execution behaviour stored under `[tools]` in `newt.toml` (#726).
///
/// Tool-output knobs: the **token budget** that caps every tool's model-facing
/// output, plus the head/tail split for oversized shell output. This bounds
/// what a single tool result can add to the context window — applied to both
/// `read_file` (its char backstop) and `run_command` (its shell envelope) — so a
/// verbose command or a huge file can't saturate a small local model's window
/// and abandon the task. Mirrors Codex's `exec_command.max_output_tokens`.
/// Distinct from `[tui] spill_lines`, which bounds collapsed live and completed
/// on-screen results, and the legacy `tool_output_lines` pre-execution preview
/// limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Maximum tokens of model-facing output any single tool may return before
    /// it is truncated (with a marker steering the model to a narrower command /
    /// a paginated read). Default: 10000. `0` disables the cap. Tokens are
    /// estimated with the shared chars/token heuristic (see [`crate::tokens`]).
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: usize,

    /// Tokens reserved for the head of an oversized `run_command` result. The
    /// remaining budget is spent on the tail, so failures and summaries at the
    /// end survive by default. `0` is pure-tail; values greater than
    /// `max_output_tokens` are clamped to pure-head.
    #[serde(default = "default_output_head_tokens")]
    pub output_head_tokens: usize,

    /// Conservative chars/token used to SIZE the output cap — distinct from the
    /// 4 c/t context estimate. Dense tool output (hex, base64, minified JSON,
    /// columnar data) tokenizes denser than prose, so a cap sized at 4 c/t
    /// admits more real tokens than its budget and can overrun a served window
    /// on its own. Default 3 keeps a capped result at/under its token budget for
    /// dense content. Lower it (e.g. 2) for very dense workloads; 4 matches the
    /// estimate (no conservatism). Clamped to a minimum of 1.
    #[serde(default = "default_output_cap_chars_per_token")]
    pub output_cap_chars_per_token: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: default_max_output_tokens(),
            output_head_tokens: default_output_head_tokens(),
            output_cap_chars_per_token: default_output_cap_chars_per_token(),
        }
    }
}

pub(crate) fn default_max_output_tokens() -> usize {
    10_000
}

pub(crate) fn default_output_head_tokens() -> usize {
    1_500
}

pub(crate) fn default_output_cap_chars_per_token() -> usize {
    3
}
