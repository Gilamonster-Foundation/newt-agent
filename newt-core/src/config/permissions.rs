//! Permission/posture declarations, caveat lowering, and persistent network grants.
//!
//! These policies describe admitted authority; the host lowers them into signed
//! capabilities for enforcement. Postures bind skill, preset, and framing atomically.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Config;
use crate::error::{NewtError, Result};

/// One named permission-posture binding (`[modes.<name>]`, retained for
/// compatibility): the atomic binding `/posture <name>` applies.
///
/// ```toml
/// [modes.triage]
/// skill   = "oncall-triage"        # skill body to preload (use_skill path)
/// preset  = "readonly-triage"      # [permission_presets.<name>] to clamp to
/// framing = "On-call triage: investigate, do not change production."
/// ```
///
/// Every field is optional so a posture can do any subset (e.g. preset-only, or
/// framing-only). A `skill`/`preset` that names a missing entry is reported as
/// an error by the command rather than silently ignored — a posture that claims
/// a clamp it never applied would be a false security claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModeConfig {
    /// Skill name to preload (the same `use_skill` / `load_body_from` path).
    /// `None` ⇒ no skill is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// `[permission_presets.<name>]` to apply as the session authority floor.
    /// `None` ⇒ authority unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// One-line framing injected into the system prompt. `None` ⇒ no framing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool permissions — preset policies, lowered to attenuated capabilities
// ---------------------------------------------------------------------------

/// A named tool-permission preset for the TUI tool loop.
///
/// Each preset selects a [`crate::Caveats`] *policy* via
/// [`ToolPermissions::to_caveats`]; the host (`newt-identity`) then lowers that
/// policy into a signed, attenuation-only capability for enforcement. A preset
/// is a name-based convenience, **not** a capability itself — the unforgeable
/// authority is the signed `AgentKey` delegation. `Custom` means the user has
/// added commands beyond a canned preset; it carries `WorkspaceDev` authority
/// plus those extras (it does **not** grant full access).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPreset {
    /// Read files and list dirs only; no writes, no commands.
    ReadOnly,
    /// Read + write within the workspace; no shell commands.
    WorkspaceEdit,
    /// Read, write workspace, run a conservative set of dev tools.
    /// See [`ToolPermissions::to_caveats`] for the exact allowlist.
    #[default]
    WorkspaceDev,
    /// Unrestricted — `Caveats::top()`. `write_file` still prompts y/N.
    FullAccess,
    /// User has added commands beyond a canned preset; carries `WorkspaceDev`
    /// authority plus those `extra_exec` entries — **not** full access.
    Custom,
}

impl PermissionPreset {
    pub const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::WorkspaceEdit,
        Self::WorkspaceDev,
        Self::FullAccess,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceEdit => "workspace_edit",
            Self::WorkspaceDev => "workspace_dev",
            Self::FullAccess => "full_access",
            Self::Custom => "custom",
        }
    }

    /// Cycle through the four user-visible presets (skips `Custom`).
    pub fn toggle(&self) -> Self {
        let idx = Self::ALL.iter().position(|p| p == self).unwrap_or(2);
        Self::ALL[(idx + 1) % Self::ALL.len()].clone()
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read files + list dirs; no writes, no commands",
            Self::WorkspaceEdit => "read + write workspace; no shell commands",
            Self::WorkspaceDev => "read, write workspace, run: cargo just git grep rg fd ...",
            Self::FullAccess => "unrestricted (prompts y/N before each write)",
            Self::Custom => "workspace-dev tools plus your extra commands",
        }
    }
}

/// Permission configuration stored under `[tui.permissions]` in `newt.toml`.
///
/// Call [`ToolPermissions::to_caveats`] to obtain the runtime [`crate::Caveats`]
/// enforced by every `execute_tool` dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPermissions {
    /// The active preset.
    pub preset: PermissionPreset,

    /// Extra commands allowed beyond the `WorkspaceDev` built-in set.
    /// Only consulted when `preset == WorkspaceDev` or `Custom`.
    /// Stored as leading tokens, e.g. `["bacon", "make"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_exec: Vec<String>,

    /// Hosts the agent may reach with `web_fetch` (the `net` capability axis).
    ///
    /// Empty (the default) = **no network** — `web_fetch` is denied. A single
    /// `"*"` grants **all** hosts (still SSRF-screened + DNS-rebind-pinned by the
    /// web tool). Otherwise an exact host allowlist, e.g.
    /// `["docs.rs", "raw.githubusercontent.com"]`. Applies to every preset
    /// except `FullAccess` (which is already unrestricted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<String>,

    /// Prompt the human when a tool call is denied by the session's caveats
    /// (issue #263): allow once / allow for this session / deny. Default
    /// **false** — a denial fails the call exactly as before (deny-by-default
    /// stays the posture). Interactive TUI only; headless paths (ACP worker,
    /// `newt-eval`) never prompt regardless. Also enabled per-run via the
    /// `--prompt-for-permissions` CLI flag. Every prompted decision is
    /// recorded to `~/.newt/permission-log.jsonl` for later review.
    #[serde(default)]
    pub prompt: bool,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        Self {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: Vec::new(),
            net: Vec::new(),
            prompt: false,
        }
    }
}

impl ToolPermissions {
    /// Built-in exec allowlist for the `WorkspaceDev` preset.
    const WORKSPACE_DEV_EXEC: &'static [&'static str] = &[
        "cargo",
        // rustc must be here: cargo spawns it as a subprocess to compile and
        // test. Without it, `cargo test` fails with "could not execute rustc".
        // rustfmt and clippy-driver are already present; this was an oversight.
        "rustc",
        "just",
        "git",
        "grep",
        "rg",
        "ripgrep",
        "fd",
        "find",
        "cat",
        "ls",
        "echo",
        "pwd",
        "true",
        "false",
        "head",
        "tail",
        "wc",
        "sort",
        "uniq",
        "diff",
        "patch",
        "rustfmt",
        "clippy-driver",
        "rustup",
        // Polyglot dev tools reached for routinely in a mixed workspace. Same
        // risk tier as cargo/git — WorkspaceDev already grants workspace write
        // and the full Rust toolchain. Anything outside this set can still be
        // opted in per-config via `[tui.permissions] extra_exec = [...]`.
        "gh",
        "python",
        "python3",
        "pip",
        "npm",
        "node",
        "make",
        "jq",
        "curl",
        "awk",
        "sed",
        "cut",
        "xargs",
        "which",
        "env",
    ];

    /// Build the runtime `Caveats` for this permission configuration.
    ///
    /// `workspace` is the absolute path to the current workspace directory;
    /// it is stored in `Scope::Only` so the TUI enforcement layer can do
    /// prefix matching (path within workspace → permitted).
    ///
    /// Note: the `Caveats` lattice uses exact-set semantics; prefix matching
    /// is the responsibility of the enforcement site (`tui_permits_path` in
    /// newt-tui), not this algebra. This is an intentional layer separation.
    pub fn to_caveats(&self, workspace: &str) -> crate::caveats::Caveats {
        use crate::caveats::{Caveats, CountBound, Scope};

        let ws = workspace.to_string();
        let net = self.net_scope();

        match self.preset {
            PermissionPreset::ReadOnly => Caveats {
                fs_read: Scope::All,
                fs_write: Scope::none(),
                exec: Scope::none(),
                net,
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            },

            PermissionPreset::WorkspaceEdit => Caveats {
                fs_read: Scope::All,
                fs_write: Scope::only([ws]),
                exec: Scope::none(),
                net,
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            },

            // `Custom` shares this arm: editing `extra_exec` keeps WorkspaceDev
            // authority plus the added commands. It must NOT escalate to
            // `top()` — adding one command to an allowlist should never grant
            // full access.
            PermissionPreset::WorkspaceDev | PermissionPreset::Custom => {
                let mut allowed: std::collections::BTreeSet<String> = Self::WORKSPACE_DEV_EXEC
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                for cmd in &self.extra_exec {
                    allowed.insert(cmd.clone());
                }
                Caveats {
                    fs_read: Scope::All,
                    fs_write: Scope::only([ws]),
                    exec: Scope::Only(allowed),
                    net,
                    max_calls: CountBound::Unlimited,
                    valid_for_generation: Scope::All,
                }
            }

            PermissionPreset::FullAccess => Caveats::top(),
        }
    }

    /// Lower the configured `net` allowlist into a capability [`Scope`].
    ///
    /// Empty → `none` (no network). A `"*"` entry → `All` (every host, still
    /// SSRF-screened by the web tool). Otherwise an exact host allowlist.
    pub(super) fn net_scope(&self) -> crate::caveats::Scope<String> {
        use crate::caveats::Scope;
        if self.net.is_empty() {
            Scope::none()
        } else if self.net.iter().any(|h| h == "*") {
            Scope::All
        } else {
            Scope::only(self.net.iter().cloned())
        }
    }
}

impl Config {
    /// The confined leash MCP *probe* children run under — shared by
    /// `newt doctor` and `newt mcp probe` (#1292): the operator's configured
    /// `[tui]` permissions preset, or a **ReadOnly, no-prompt default** when
    /// none is configured — the session's "safe by default, never `top()`"
    /// rule (#94). The spawn path widens exec by exactly the probed command
    /// (`newt-mcp-client`'s `spawn_caveats`); everything else stays closed.
    #[must_use]
    pub fn mcp_probe_caveats(&self, workspace: &Path) -> crate::caveats::Caveats {
        let ws = workspace.to_string_lossy();
        self.tui
            .as_ref()
            .map(|t| t.permissions.to_caveats(&ws))
            .unwrap_or_else(|| {
                ToolPermissions {
                    preset: PermissionPreset::ReadOnly,
                    extra_exec: Vec::new(),
                    net: Vec::new(),
                    prompt: false,
                }
                .to_caveats(&ws)
            })
    }

    /// #904: append `host` to `[tui.permissions] net` in the TOML `text`,
    /// **preserving comments and formatting** — unlike [`Config::save`], which
    /// re-serializes the whole typed struct and drops the user's comments,
    /// ordering, and any keys newt does not model. Creates the
    /// `[tui.permissions]` table and its `net` array if absent; a no-op if the
    /// host is already listed. PURE (no I/O), so it unit-tests without a
    /// filesystem. This is the durable "allow permanently" grant path — it is
    /// only ever driven by an explicit human keypress at the permission prompt.
    pub fn with_net_host(text: &str, host: &str) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        let tui = doc
            .as_table_mut()
            .entry("tui")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let tui_tbl = tui
            .as_table_mut()
            .ok_or_else(|| NewtError::Config("[tui] is not a table".to_string()))?;
        let perms = tui_tbl
            .entry("permissions")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let perms_tbl = perms
            .as_table_mut()
            .ok_or_else(|| NewtError::Config("[tui.permissions] is not a table".to_string()))?;
        let net =
            perms_tbl
                .entry("net")
                .or_insert(toml_edit::Item::Value(toml_edit::Value::Array(
                    toml_edit::Array::new(),
                )));
        let arr = net.as_array_mut().ok_or_else(|| {
            NewtError::Config("[tui.permissions] net is not an array".to_string())
        })?;
        if !arr.iter().any(|v| v.as_str() == Some(host)) {
            arr.push(host);
        }
        Ok(doc.to_string())
    }

    /// Durably grant a net host by appending it to `[tui.permissions] net` in the
    /// config file at `path`, comment-preserving (see [`Config::with_net_host`]).
    /// A missing file is treated as empty (the table is created). Creates parent
    /// dirs as needed. Used by the interactive gate's "allow permanently" choice.
    pub fn append_permission_net_host(path: &Path, host: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let destination = crate::atomic_fs::ResolvedPath::resolve(path).map_err(|error| {
            NewtError::Config(format!(
                "resolve config destination for {}: {error:#}",
                path.display()
            ))
        })?;
        let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path())
            .map_err(|error| NewtError::Config(format!("lock {}: {error:#}", path.display())))?;
        let text = match std::fs::read_to_string(destination.as_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(NewtError::Io(error)),
        };
        let updated = Self::with_net_host(&text, host)?;
        destination
            .atomic_write(updated.as_bytes())
            .map_err(|error| NewtError::Config(format!("write {}: {error:#}", path.display())))
    }
}
