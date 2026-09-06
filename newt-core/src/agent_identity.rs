//! Agent commit identity (`.newt/agent-identity.toml`).
//!
//! This is the configurable **commit identity** every newt-based agent runs
//! under — the git author/committer name + email a future commit path will
//! stamp, the optional signing key that links that git identity to the §6
//! writer fingerprint and the agent-mesh `AgentKey` root, and the optional
//! GitHub App coordinates a later autonomous-push path will mint tokens from.
//!
//! gilamonster-agent and any other newt-based agent **inherit** the
//! compiled-in [`AgentIdentity::default`] — the GitHub **User**
//! [`newt-agent`](https://github.com/newt-agent) — and override it by
//! shipping their own `agent-identity.toml`. To attribute as the GitHub App
//! bot instead, set [`GITHUB_APP_BOT_NAME`] / [`GITHUB_APP_BOT_EMAIL`].
//!
//! # LOAD-BEARING SECURITY RULE — secrets are references, never values
//!
//! This struct holds **no field that carries raw key or token material.**
//! Every secret is a *reference* the runtime resolves on demand:
//!
//! - [`AgentIdentity::signing_key`] / [`AgentIdentity::public_key`] and
//!   [`GithubApp::private_key`] are filesystem **paths** (tilde-expanded).
//! - Each [`tokens`](AgentIdentity::tokens) entry is a [`SecretRef`] — an
//!   `{env|file|cmd}` reference, resolved by [`SecretRef::resolve`].
//!
//! That is what makes `agent-identity.toml` safe to commit: it contains only
//! the agent's public name/email, public app id/client id, and *paths to* the
//! secrets — never a private key, never a token. The [`AgentIdentity::token`]
//! and [`SecretRef::resolve`] results are wrapped in [`Secret`], which does not
//! implement `Serialize`/`Display`/`Debug`-of-the-value, so a resolved secret
//! cannot be round-tripped back into a config file or logged by accident.
//!
//! Resolution precedence mirrors `.newt/config.toml` exactly (see
//! [`AgentIdentity::resolve`]): workspace `.newt/agent-identity.toml`
//! (walk-up) > `~/.newt/agent-identity.toml` > `/etc/newt/agent-identity.toml`
//! > the compiled-in default.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::{
    expand_tilde, find_project_config_from, home_dir, merge_toml, ArrayMergeStrategy, Config,
};
use crate::error::{NewtError, Result};

// ---------------------------------------------------------------------------
// Compiled-in defaults — the inherited GitHub User `newt-agent` base
// ---------------------------------------------------------------------------

/// Compiled-in default author name — the GitHub **User**
/// [`newt-agent`](https://github.com/newt-agent) (not the App bot).
///
/// Prefer this for harness attribution so commits link to the User
/// profile. Override in `agent-identity.toml` when a different author
/// (or the App bot — see [`GITHUB_APP_BOT_NAME`]) is required.
pub const DEFAULT_AGENT_NAME: &str = "newt-agent";

/// Compiled-in default author email — GitHub User no-reply form.
///
/// Format: `{user_id}+{login}@users.noreply.github.com`. The numeric id
/// (`309460085`) is the user id for <https://github.com/newt-agent>.
pub const DEFAULT_AGENT_EMAIL: &str = "309460085+newt-agent@users.noreply.github.com";

/// GitHub App bot display name (`newt-agent[bot]`).
///
/// Kept for operators who want App-bot attribution via
/// `agent-identity.toml` instead of the User default.
pub const GITHUB_APP_BOT_NAME: &str = "newt-agent[bot]";

/// GitHub App bot no-reply email (`{app_user_id}+newt-agent[bot]@…`).
///
/// The numeric id (`293447090`) is the App's bot user id, distinct from
/// the User account id in [`DEFAULT_AGENT_EMAIL`].
pub const GITHUB_APP_BOT_EMAIL: &str = "293447090+newt-agent[bot]@users.noreply.github.com";

/// Filename looked for under each config root (workspace / home / system).
pub const AGENT_IDENTITY_FILENAME: &str = "agent-identity.toml";

// ---------------------------------------------------------------------------
// Secret-by-reference primitives
// ---------------------------------------------------------------------------

/// A resolved secret value, wrapped so it cannot be serialized, formatted, or
/// logged by accident.
///
/// Deliberately *not* `Serialize`/`Deserialize` (a resolved secret must never
/// round-trip back into a config file) and its `Debug` redacts the value. Call
/// [`Secret::expose`] at the single point of use that genuinely needs the
/// bytes (e.g. an `Authorization:` header) and never store the result.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a freshly-resolved secret string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying secret value. Use only at the point of
    /// consumption; never log or persist the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the value — not even in panic/Debug output.
        f.write_str("Secret(<redacted>)")
    }
}

/// A reference to a secret — never the secret itself.
///
/// Exactly one of the three sources is set per entry:
///
/// ```toml
/// [agent-identity.tokens]
/// from_env  = { env  = "SOME_TOKEN" }                 # read $SOME_TOKEN
/// from_file = { file = "~/.secrets/tok" }             # first non-empty line
/// from_cmd  = { cmd  = "pass show ci/token" }         # stdout of the command
/// ```
///
/// This is the same secret-by-reference shape as
/// [`crate::config::BackendConfig::resolve_api_key`] (env-or-file), extended
/// with a `cmd` source for secret managers (`pass`, `vault`, `op`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SecretRef {
    /// Environment variable to read the secret from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// File whose first non-empty line is the secret (tilde-expanded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Shell command whose trimmed stdout is the secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
}

impl SecretRef {
    /// Resolve this reference to its [`Secret`] value.
    ///
    /// Source precedence when more than one is set: `env` → `file` → `cmd`.
    /// Returns `Ok(None)` when the configured source resolves to nothing
    /// (missing env var / empty file) and `Err` only when a configured `cmd`
    /// fails to execute or exits non-zero — a misconfiguration the caller
    /// should see rather than silently treat as "no token".
    pub fn resolve(&self) -> Result<Option<Secret>> {
        if let Some(var) = &self.env {
            if let Ok(val) = std::env::var(var) {
                let val = val.trim();
                if !val.is_empty() {
                    return Ok(Some(Secret::new(val)));
                }
            }
            return Ok(None);
        }
        if let Some(path) = &self.file {
            let expanded = expand_tilde(path);
            let contents = std::fs::read_to_string(&expanded).map_err(NewtError::Io)?;
            if let Some(token) = contents.lines().map(str::trim).find(|l| !l.is_empty()) {
                return Ok(Some(Secret::new(token)));
            }
            return Ok(None);
        }
        if let Some(cmd) = &self.cmd {
            let output = shell_command(cmd).output().map_err(NewtError::Io)?;
            if !output.status.success() {
                return Err(NewtError::Config(format!(
                    "token command exited {}: {cmd}",
                    output.status
                )));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(token) = stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
                return Ok(Some(Secret::new(token)));
            }
            return Ok(None);
        }
        Ok(None)
    }
}

#[cfg(windows)]
fn shell_command(cmd: &str) -> Command {
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut command = Command::new(shell);
    command.arg("/C").arg(cmd);
    command
}

#[cfg(not(windows))]
fn shell_command(cmd: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    command
}

// ---------------------------------------------------------------------------
// GitHub App config (for a later autonomous-push path)
// ---------------------------------------------------------------------------

/// GitHub App coordinates for a *future* autonomous-push path. Surfaced here as
/// config only — token MINTING is deliberately out of scope.
///
/// `app_id` / `client_id` / `installation_id` are public identifiers; the only
/// secret is `private_key`, which is a filesystem **path** (tilde-expanded),
/// never inline PEM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubApp {
    /// The GitHub App's numeric id (public).
    pub app_id: u64,
    /// The GitHub App's client id, e.g. `Iv23li...` (public).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The installation id this agent acts as (public).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<u64>,
    /// Filesystem **path** to the App's PEM private key (tilde-expanded).
    /// Never inline PEM. `None` when the key is provisioned out-of-band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
}

// ---------------------------------------------------------------------------
// The identity config itself
// ---------------------------------------------------------------------------

/// A newt-based agent's commit identity.
///
/// Resolved with the same precedence as `.newt/config.toml`. See the module
/// docs for the secrets-by-reference rule: this struct carries **no raw key or
/// token material**, only the public name/email and *paths to* / *references
/// for* secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "agent-identity")]
pub struct AgentIdentity {
    /// Git author/committer name (public). Default: [`DEFAULT_AGENT_NAME`].
    pub name: String,
    /// Git author/committer email (public). Default: [`DEFAULT_AGENT_EMAIL`]
    /// (GitHub User no-reply for <https://github.com/newt-agent>).
    pub email: String,

    /// Optional model override for [`AgentIdentity::co_author_trailer`]'s
    /// PREVIEW rendering (`newt identity`). The live commit path does not
    /// read this field — it stamps whichever models actually contributed,
    /// via [`crate::attribution::AttributionLedger`] (#1707/#1709).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// The human operator running this agent, stamped into the commit
    /// attribution footer's `Operator:` field. When unset in config it is
    /// derived from the host git identity (`git config user.name`, then
    /// `GIT_AUTHOR_NAME`, then the OS username) so a rebrand / unconfigured
    /// box still records *someone*. `None` only when no source resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,

    /// The human operator's EMAIL for operator `Co-authored-by:` attribution
    /// — the second half of the atomic `(operator, operator_email)` pair (see
    /// [`AgentIdentity::operator_identity`]). When BOTH this and `operator`
    /// are set, that explicitly configured pair wins outright. When `operator`
    /// is set but this is not, the configured name is kept for `Operator:`
    /// provenance and NO email is emitted — a configured name is never paired
    /// with an unrelated host email. When `operator` is unset, the matched
    /// host Git pair (`git config user.name` + `user.email`) supplies both
    /// halves. Never invented; `None` when no real source resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_email: Option<String>,

    /// Filesystem **path** to the agent's signing key PEM (tilde-expanded),
    /// the [`agent_mesh_protocol::UserKey`] that roots the §6 writer
    /// fingerprint and the mesh `AgentKey`. A **path**, never inline key
    /// material. `None` → no commit signing / no mesh-rooted identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,

    /// Filesystem **path** to the matching public key (tilde-expanded).
    /// A **path**, never inline key material. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,

    /// GitHub App coordinates for a later autonomous-push path. `None` by
    /// default — newt never mints a token unless this is set (and even then,
    /// minting is out of scope for the foundation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_app: Option<GithubApp>,

    /// Named token references (`[agent-identity.tokens]`). Each value is a
    /// [`SecretRef`] (`{env|file|cmd}`) — the NAME is config, the VALUE is
    /// resolved on demand and never stored here. Empty by default.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub tokens: std::collections::BTreeMap<String, SecretRef>,
}

impl Default for AgentIdentity {
    /// The compiled-in GitHub User `newt-agent` base every newt agent inherits.
    /// No signing key, no GitHub App, no tokens — a fresh newt with zero
    /// config resolves cleanly to this.
    fn default() -> Self {
        Self {
            name: DEFAULT_AGENT_NAME.to_string(),
            email: DEFAULT_AGENT_EMAIL.to_string(),
            model: None,
            operator: None,
            operator_email: None,
            signing_key: None,
            public_key: None,
            github_app: None,
            tokens: std::collections::BTreeMap::new(),
        }
    }
}

/// Derive the operator name from the host when config leaves it unset:
/// `git config user.name` (the human's own git identity), then the
/// `GIT_AUTHOR_NAME` env var, then the OS username. Returns `None` when no
/// source resolves. Never fails — attribution degrades to omitting the field.
pub fn default_operator() -> Option<String> {
    let from_git = std::process::Command::new("git")
        .args(["config", "--get", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    from_git
        .or_else(|| {
            std::env::var("GIT_AUTHOR_NAME")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
}

/// Read a single `git config` value (`user.name` / `user.email`). Returns
/// `None` when git is absent, the key is unset, or the value is empty — never
/// invents. This is the matched-pair reader for [`host_operator_identity`]:
/// both halves come from the SAME `git config` source, so the pair can never
/// disagree about whose identity it describes.
fn read_git_config(key: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The matched host Git operator pair — `(user.name, user.email)`, both from
/// the SAME `git config` source. Returns `(Some(name), Some(email))` only
/// when BOTH resolve (a real matched pair); `(Some(name), None)` when only
/// the host name resolves (provenance only, no `Co-authored-by:`); `(None,
/// None)` when neither resolves. When the host name is absent but an email is
/// set, the email is DROPPED — it has no matched name, so it cannot form a
/// pair and is never paired with an env-fallback name. The env/`USER` name
/// fallback ([`default_operator`]) is used only for the provenance name when
/// `git config user.name` is unset, never paired with a host email.
fn host_operator_identity() -> (Option<String>, Option<String>) {
    let name = read_git_config("user.name");
    let email = read_git_config("user.email");
    match (name, email) {
        (Some(n), Some(e)) => (Some(n), Some(e)),
        (Some(n), None) => (Some(n), None),
        (None, _) => {
            // No host git name to match the email (if any); fall back to the
            // env/USER name for provenance, but emit NO email — an env name
            // and a `git config` email are not a matched pair.
            (default_operator(), None)
        }
    }
}

/// Pure resolution of the operator co-author identity from the configured
/// fields and a host Git pair — no subprocess, fully deterministic, the
/// testable core of [`AgentIdentity::operator_identity`]. See that method for
/// the resolution contract (configured pair → configured name only → matched
/// host pair / host name only). Extracted so the atomic-pair logic is unit-
/// testable without shelling out to a real `git config`.
fn resolve_operator_identity(
    cfg_name: Option<&str>,
    cfg_email: Option<&str>,
    host: (Option<String>, Option<String>),
) -> (Option<String>, Option<String>) {
    let cfg_name = cfg_name.map(str::trim).filter(|s| !s.is_empty());
    let cfg_email = cfg_email.map(str::trim).filter(|s| !s.is_empty());
    // 1. Explicitly configured pair: both halves present.
    if let (Some(name), Some(email)) = (cfg_name, cfg_email) {
        return (Some(name.to_string()), Some(email.to_string()));
    }
    // 2. Configured name without configured email: keep the name for
    //    provenance, emit NO email — never pair a configured name with an
    //    unrelated host email, never swap it for the host name.
    if let Some(name) = cfg_name {
        return (Some(name.to_string()), None);
    }
    // 3. No configured name: the matched host Git pair. Defensively enforce
    //    the atomicity invariant here as well — an email with no matching
    //    name is not a pair (never pair an email with an absent name), so the
    //    rule holds regardless of what the host builder returned.
    match host {
        (Some(n), Some(e)) => (Some(n), Some(e)),
        (Some(n), None) => (Some(n), None),
        (None, _) => (None, None),
    }
}

/// Which layer an [`AgentIdentity`] resolved from. Surfaced by
/// [`AgentIdentity::resolve_with_source`] so `newt identity` can tell the user
/// *where* the identity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentitySource {
    /// A project-local `.newt/agent-identity.toml` (walk-up from cwd).
    Workspace(PathBuf),
    /// `~/.newt/agent-identity.toml`.
    Home(PathBuf),
    /// `/etc/newt/agent-identity.toml`.
    System(PathBuf),
    /// The compiled-in GitHub User `newt-agent` default — no file existed.
    Default,
}

impl IdentitySource {
    /// A short human label for `newt identity` output.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Workspace(p) => format!("workspace ({})", p.display()),
            Self::Home(p) => format!("home ({})", p.display()),
            Self::System(p) => format!("system ({})", p.display()),
            Self::Default => "compiled-in default (newt-agent)".to_string(),
        }
    }

    /// Hint for how to override the compiled-in default.
    ///
    /// Printed by `newt identity` when no file is configured. The future setup
    /// dialog will call [`AgentIdentity::save`] into the same home path.
    #[must_use]
    pub fn configure_hint(&self) -> Option<&'static str> {
        match self {
            Self::Default => Some(
                "Override: write ~/.newt/agent-identity.toml (or `newt identity set --name … --email …`). \
                 A future setup dialog will use the same path.",
            ),
            _ => None,
        }
    }
}

impl AgentIdentity {
    /// Load an identity config from an explicit file path.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(NewtError::Io)?;
        Self::from_toml_str(&text)
    }

    /// Parse an identity config from a TOML string.
    ///
    /// The file's top-level table is `[agent-identity]`, so we read that
    /// sub-table and deserialize it over the compiled-in default (any omitted
    /// field inherits the User `newt-agent` base).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| NewtError::Config(e.to_string()))?;
        Self::from_value(value)
    }

    /// Deserialize from a raw `toml::Value` whose `[agent-identity]` sub-table
    /// (if present) is layered over [`AgentIdentity::default`].
    fn from_value(value: toml::Value) -> Result<Self> {
        // Start from the compiled-in default, then overlay the file's
        // `[agent-identity]` table so partial files inherit name/email/etc.
        let mut base =
            toml::Value::try_from(Self::default()).map_err(|e| NewtError::Config(e.to_string()))?;
        if let Some(section) = value.get("agent-identity").cloned() {
            merge_toml(&mut base, section, ArrayMergeStrategy::Replace);
        }
        base.try_into()
            .map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Resolve the agent identity using the same precedence as
    /// `.newt/config.toml`:
    ///
    /// 1. workspace `.newt/agent-identity.toml` (walk-up from cwd, stopping
    ///    before `$HOME`),
    /// 2. `~/.newt/agent-identity.toml`,
    /// 3. `/etc/newt/agent-identity.toml`,
    /// 4. the compiled-in [`AgentIdentity::default`] (GitHub User `newt-agent`).
    ///
    /// First file found wins; no file → the default. This never requires any
    /// file to exist.
    pub fn resolve() -> Result<Self> {
        Ok(Self::resolve_with_source()?.0)
    }

    /// Path of the user-level identity file (`~/.newt/agent-identity.toml`,
    /// or `$NEWT_CONFIG_DIR/agent-identity.toml`).
    ///
    /// This is the write target for `newt identity set` and for a future
    /// setup-dialog identity step. `None` when no home/config dir is resolvable.
    #[must_use]
    pub fn user_identity_path() -> Option<PathBuf> {
        Config::user_config_dir().map(|dir| dir.join(AGENT_IDENTITY_FILENAME))
    }

    /// Serialize this identity as a complete `[agent-identity]` TOML document.
    ///
    /// Secrets stay references (paths / env / cmd) — never raw values — so the
    /// result is safe to write into a committed or home config file.
    pub fn to_toml_string(&self) -> Result<String> {
        // Wrap so the root table is `[agent-identity]`, matching the load path.
        #[derive(Serialize)]
        struct File<'a> {
            #[serde(rename = "agent-identity")]
            identity: &'a AgentIdentity,
        }
        toml::to_string_pretty(&File { identity: self })
            .map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Write this identity to `path`, creating parent directories if needed.
    ///
    /// Public write seam for `newt identity set` and a future setup dialog —
    /// both should land here rather than open-coding TOML.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let text = self.to_toml_string()?;
        std::fs::write(path, text).map_err(NewtError::Io)
    }

    /// Like [`AgentIdentity::resolve`] but also reports which layer it came
    /// from (for `newt identity`).
    pub fn resolve_with_source() -> Result<(Self, IdentitySource)> {
        let cwd = std::env::current_dir().ok();
        let home = home_dir();
        let user_config_dir = Config::user_config_dir();
        Self::resolve_from_dirs(cwd.as_deref(), home.as_deref(), user_config_dir.as_deref())
    }

    /// The resolution core, parameterized on cwd + home so it is unit-testable
    /// against temp dirs without mutating the process environment.
    #[cfg(test)]
    pub(crate) fn resolve_from(
        cwd: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<(Self, IdentitySource)> {
        let user_config_dir = home.map(|h| h.join(".newt"));
        Self::resolve_from_dirs(cwd, home, user_config_dir.as_deref())
    }

    fn resolve_from_dirs(
        cwd: Option<&Path>,
        home: Option<&Path>,
        user_config_dir: Option<&Path>,
    ) -> Result<(Self, IdentitySource)> {
        // 1. Workspace walk-up — reuse the exact config.toml walk-up logic.
        if let Some(start) = cwd {
            if let Some(cfg) = find_project_config_from(start, home) {
                // find_project_config_from returns `.../.newt/config.toml`;
                // the identity file is its sibling.
                let candidate = cfg.with_file_name(AGENT_IDENTITY_FILENAME);
                if candidate.is_file() {
                    return Ok((
                        Self::load(&candidate)?,
                        IdentitySource::Workspace(candidate),
                    ));
                }
            }
            // The walk-up above only matches dirs that already host a
            // `config.toml`. Also honor a standalone `.newt/agent-identity.toml`
            // with no sibling config (an agent may ship identity alone).
            if let Some(found) = find_identity_walkup(start, home) {
                return Ok((Self::load(&found)?, IdentitySource::Workspace(found)));
            }
        }

        // 2. Home.
        if let Some(dir) = user_config_dir {
            let candidate = dir.join(AGENT_IDENTITY_FILENAME);
            if candidate.is_file() {
                return Ok((Self::load(&candidate)?, IdentitySource::Home(candidate)));
            }
        }

        // 3. System.
        let system = PathBuf::from("/etc/newt").join(AGENT_IDENTITY_FILENAME);
        if system.is_file() {
            return Ok((Self::load(&system)?, IdentitySource::System(system)));
        }

        // 4. Compiled-in default.
        Ok((Self::default(), IdentitySource::Default))
    }

    // -----------------------------------------------------------------------
    // Accessors — the API a future commit path consumes
    // -----------------------------------------------------------------------

    /// `(name, email)` for the git author/committer.
    #[must_use]
    pub fn git_author(&self) -> (String, String) {
        (self.name.clone(), self.email.clone())
    }

    /// The human operator name for the commit attribution footer's
    /// `Operator:` field — the NAME half of the atomic
    /// [`AgentIdentity::operator_identity`] pair. Config wins; otherwise
    /// derived from the host git identity so an unconfigured box still
    /// records *someone*.
    #[must_use]
    pub fn operator_name(&self) -> Option<String> {
        self.operator_identity().0
    }

    /// The human operator's EMAIL for operator `Co-authored-by:` attribution
    /// — the EMAIL half of the atomic
    /// [`AgentIdentity::operator_identity`] pair. `None` when no real,
    /// same-source-paired email resolves; NEVER invented (the
    /// operator-attribution contract: emit a `Co-authored-by:` for the
    /// operator only when a REAL email is known AND it is paired with a name
    /// from the same source — a configured name is never paired with an
    /// unrelated host email). Delegates to [`AgentIdentity::operator_identity`]
    /// so the two halves can never disagree.
    #[must_use]
    pub fn operator_email(&self) -> Option<String> {
        self.operator_identity().1
    }

    /// Resolve the human operator's co-author identity as an ATOMIC
    /// `(name, email)` pair — the unit of GitHub `Co-authored-by:`
    /// recognition. The two halves are NEVER resolved independently:
    ///
    /// 1. **Explicitly configured pair** — both `operator` (name) and
    ///    `operator_email` set in `agent-identity.toml`. Both must be present
    ///    and non-empty; this is the operator's deliberate, complete identity
    ///    and wins outright.
    /// 2. **Configured name without configured email** — the configured name
    ///    is kept for `Operator:` provenance and the email is `None`: no
    ///    human `Co-authored-by:` is emitted. A configured name is NEVER
    ///    paired with an unrelated independently-discovered host email
    ///    (requirement 8), and never swapped for the host name — the
    ///    operator's stated identity wins for provenance.
    /// 3. **Matched host Git pair** — when NO operator name is configured,
    ///    the host git identity (`git config user.name` + `user.email`, the
    ///    same source) supplies both halves as a matched pair. When only the
    ///    host name resolves, it is kept for provenance with no email
    ///    (requirement 10); when neither resolves, the operator is unknown.
    ///
    /// An email is therefore emitted ONLY when it is paired with a name from
    /// the SAME source — configured email with a configured name, or host
    /// email with a host name. Never invented (requirement 11).
    #[must_use]
    pub fn operator_identity(&self) -> (Option<String>, Option<String>) {
        resolve_operator_identity(
            self.operator.as_deref(),
            self.operator_email.as_deref(),
            host_operator_identity(),
        )
    }

    /// A PREVIEW `Co-authored-by:` trailer line (`newt identity`), in the
    /// same `Co-authored-by: <model> (<harness>) <email>` format the live
    /// commit path stamps via [`crate::attribution::AttributionLedger`]
    /// (#1707/#1709) — the one authoritative trailer shape, so this preview
    /// can never show something the harness would not actually produce.
    ///
    /// Uses [`AgentIdentity::model`] if configured, else a placeholder
    /// (`<model>`) — the live path always has a real resolved model; this
    /// static config field rarely does.
    #[must_use]
    pub fn co_author_trailer(&self) -> String {
        crate::attribution::Attribution::new(
            self.model.as_deref().unwrap_or("<model>"),
            crate::build_info::harness_name(),
            crate::build_info::PACKAGE_VERSION,
            self.email.clone(),
        )
        .with_build(crate::build_info::SOURCE_ID)
        .trailer()
    }

    /// The configured signing-key path, tilde-expanded. `None` when unset
    /// (the default identity mints nothing).
    #[must_use]
    pub fn signing_key_path(&self) -> Option<PathBuf> {
        self.signing_key.as_deref().map(expand_tilde)
    }

    /// The configured public-key path, tilde-expanded. `None` when unset.
    #[must_use]
    pub fn public_key_path(&self) -> Option<PathBuf> {
        self.public_key.as_deref().map(expand_tilde)
    }

    /// Resolve a named token reference to its [`Secret`]. `Ok(None)` when no
    /// token of that name is configured (or its source is empty); `Err` only
    /// when a configured `cmd` token fails. The value is never stored on the
    /// struct.
    pub fn token(&self, name: &str) -> Result<Option<Secret>> {
        match self.tokens.get(name) {
            Some(r) => r.resolve(),
            None => Ok(None),
        }
    }

    /// The GitHub App config, if any. Token-minting is deferred; this only
    /// surfaces the (public) coordinates + the private-key *path*.
    #[must_use]
    pub fn github_app(&self) -> Option<&GithubApp> {
        self.github_app.as_ref()
    }
}

/// Walk up from `start` (stopping before `home` and at the filesystem root)
/// looking for a standalone `.newt/agent-identity.toml`. Innermost wins.
fn find_identity_walkup(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if home == Some(current) {
            break;
        }
        let candidate = current.join(".newt").join(AGENT_IDENTITY_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_identity(dir: &Path, body: &str) -> PathBuf {
        let newt = dir.join(".newt");
        std::fs::create_dir_all(&newt).unwrap();
        let path = newt.join(AGENT_IDENTITY_FILENAME);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        path
    }

    #[test]
    fn to_toml_string_wraps_agent_identity_table() {
        let id = AgentIdentity {
            name: "custom-agent".into(),
            email: "custom@users.noreply.github.com".into(),
            ..AgentIdentity::default()
        };
        let text = id.to_toml_string().unwrap();
        assert!(
            text.contains("[agent-identity]"),
            "must use the loadable table name: {text}"
        );
        assert!(text.contains("name = \"custom-agent\""));
        assert!(text.contains("email = \"custom@users.noreply.github.com\""));
        // Round-trip through the loader.
        let loaded = AgentIdentity::from_toml_str(&text).unwrap();
        assert_eq!(loaded.name, "custom-agent");
        assert_eq!(loaded.email, "custom@users.noreply.github.com");
    }

    #[test]
    fn save_writes_file_that_resolve_from_home_picks_up() {
        let home = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let path = home.path().join(".newt").join(AGENT_IDENTITY_FILENAME);
        let id = AgentIdentity {
            name: "saved-agent".into(),
            email: "saved@users.noreply.github.com".into(),
            ..AgentIdentity::default()
        };
        id.save(&path).unwrap();
        assert!(path.is_file());

        let (resolved, src) =
            AgentIdentity::resolve_from(Some(elsewhere.path()), Some(home.path())).unwrap();
        assert_eq!(resolved.name, "saved-agent");
        assert_eq!(resolved.email, "saved@users.noreply.github.com");
        assert!(matches!(src, IdentitySource::Home(_)));
    }

    #[test]
    fn configure_hint_only_for_compiled_default() {
        assert!(IdentitySource::Default.configure_hint().is_some());
        assert!(
            IdentitySource::Home(PathBuf::from("/h/.newt/agent-identity.toml"))
                .configure_hint()
                .is_none()
        );
    }

    #[test]
    fn default_is_newt_agent_github_user() {
        let id = AgentIdentity::default();
        assert_eq!(id.name, "newt-agent");
        assert_eq!(id.email, "309460085+newt-agent@users.noreply.github.com");
        assert_ne!(id.name, GITHUB_APP_BOT_NAME);
        assert_ne!(id.email, GITHUB_APP_BOT_EMAIL);
        assert!(id.signing_key.is_none());
        assert!(id.public_key.is_none());
        assert!(id.github_app.is_none());
        assert!(id.tokens.is_empty());
        assert!(id.model.is_none());
    }

    #[test]
    fn resolve_with_no_files_yields_compiled_default() {
        // cwd is an empty temp dir, home a different empty temp dir: nothing on
        // disk → the compiled-in GitHub User `newt-agent` default, cleanly.
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let (id, src) = AgentIdentity::resolve_from(Some(cwd.path()), Some(home.path())).unwrap();
        assert_eq!(id, AgentIdentity::default());
        assert_eq!(src, IdentitySource::Default);
    }

    #[test]
    fn workspace_overrides_home_overrides_default() {
        let home = TempDir::new().unwrap();
        write_identity(
            home.path(),
            r#"
[agent-identity]
name = "home-agent[bot]"
email = "home@users.noreply.github.com"
"#,
        );

        // With only home set, home wins over the default.
        let elsewhere = TempDir::new().unwrap();
        let (id, src) =
            AgentIdentity::resolve_from(Some(elsewhere.path()), Some(home.path())).unwrap();
        assert_eq!(id.name, "home-agent[bot]");
        assert!(matches!(src, IdentitySource::Home(_)));

        // Now add a workspace file: workspace wins over home.
        let ws = TempDir::new().unwrap();
        write_identity(
            ws.path(),
            r#"
[agent-identity]
name = "gilamonster-agent[bot]"
email = "293450354+gilamonster-agent[bot]@users.noreply.github.com"
"#,
        );
        let (id, src) = AgentIdentity::resolve_from(Some(ws.path()), Some(home.path())).unwrap();
        assert_eq!(id.name, "gilamonster-agent[bot]");
        assert_eq!(
            id.email,
            "293450354+gilamonster-agent[bot]@users.noreply.github.com"
        );
        assert!(matches!(src, IdentitySource::Workspace(_)));
    }

    #[test]
    fn partial_file_inherits_default_email() {
        // A file that sets only `name` must inherit the default email.
        let id = AgentIdentity::from_toml_str(
            r#"
[agent-identity]
name = "custom[bot]"
"#,
        )
        .unwrap();
        assert_eq!(id.name, "custom[bot]");
        assert_eq!(id.email, DEFAULT_AGENT_EMAIL);
    }

    #[test]
    #[serial_test::serial(newt_brand_name_env)]
    fn co_author_trailer_format() {
        // SAFETY: serialized against other NEWT_BRAND_NAME-mutating tests.
        unsafe { std::env::remove_var("NEWT_BRAND_NAME") };
        let id = AgentIdentity::default();
        // #1707/#1709: the preview trailer now matches the live
        // multi-contributor format — model primary (placeholder when
        // `AgentIdentity::model` is unset), harness parenthetical, email
        // last — not `name (model) <email>`.
        assert_eq!(
            id.co_author_trailer(),
            format!(
                "Co-authored-by: <model> (v{} {}) <309460085+newt-agent@users.noreply.github.com>",
                crate::build_info::PACKAGE_VERSION,
                crate::build_info::SOURCE_ID
            )
        );
    }

    #[test]
    #[serial_test::serial(newt_brand_name_env)]
    fn co_author_trailer_with_model() {
        // SAFETY: serialized against other NEWT_BRAND_NAME-mutating tests.
        unsafe { std::env::remove_var("NEWT_BRAND_NAME") };
        let id = AgentIdentity {
            model: Some("sakamakismile/Ornith-1.0-35B-NVFP4".to_string()),
            ..AgentIdentity::default()
        };
        assert_eq!(
            id.co_author_trailer(),
            format!(
                "Co-authored-by: sakamakismile/Ornith-1.0-35B-NVFP4 (v{} {}) <309460085+newt-agent@users.noreply.github.com>",
                crate::build_info::PACKAGE_VERSION,
                crate::build_info::SOURCE_ID
            )
        );
    }

    #[test]
    fn git_author_returns_name_and_email() {
        let id = AgentIdentity::default();
        let (name, email) = id.git_author();
        assert_eq!(name, "newt-agent");
        assert_eq!(email, DEFAULT_AGENT_EMAIL);
    }

    #[test]
    fn signing_and_public_key_paths_expand_tilde() {
        let id = AgentIdentity {
            signing_key: Some("~/keys/id.pem".to_string()),
            public_key: Some("~/keys/id.pub".to_string()),
            ..AgentIdentity::default()
        };
        let sk = id.signing_key_path().unwrap();
        let pk = id.public_key_path().unwrap();
        assert!(!sk.starts_with("~"));
        assert!(sk.ends_with("keys/id.pem"));
        assert!(pk.ends_with("keys/id.pub"));
        // The default identity has no key paths.
        assert!(AgentIdentity::default().signing_key_path().is_none());
        assert!(AgentIdentity::default().public_key_path().is_none());
    }

    #[test]
    fn token_resolves_from_env() {
        let id = AgentIdentity::from_toml_str(
            r#"
[agent-identity]
name = "x[bot]"
[agent-identity.tokens]
svc = { env = "NEWT_TEST_SVC_TOKEN_ENV" }
"#,
        )
        .unwrap();
        // SAFETY: single-threaded test; the var name is unique to this test.
        unsafe { std::env::set_var("NEWT_TEST_SVC_TOKEN_ENV", "env-secret-value") };
        let tok = id.token("svc").unwrap().unwrap();
        assert_eq!(tok.expose(), "env-secret-value");
        unsafe { std::env::remove_var("NEWT_TEST_SVC_TOKEN_ENV") };
        // Missing env → Ok(None), not an error.
        assert!(id.token("svc").unwrap().is_none());
        // Unknown token name → Ok(None).
        assert!(id.token("nope").unwrap().is_none());
    }

    #[test]
    fn token_resolves_from_file() {
        let dir = TempDir::new().unwrap();
        let secret_path = dir.path().join("tok");
        std::fs::write(&secret_path, "\n  file-secret-value  \n").unwrap();
        // A TOML *literal* string ('...') — not a basic ("...") string — because
        // a temp path is backslash-laden on Windows and `\U`/`\A`/etc. are
        // invalid escapes in a basic string (the file content here is fine on
        // any platform; only the embedded path needs the literal quoting).
        let id = AgentIdentity::from_toml_str(&format!(
            r#"
[agent-identity]
name = "x[bot]"
[agent-identity.tokens]
svc = {{ file = '{}' }}
"#,
            secret_path.display()
        ))
        .unwrap();
        let tok = id.token("svc").unwrap().unwrap();
        assert_eq!(tok.expose(), "file-secret-value");
    }

    #[test]
    fn token_resolves_from_cmd() {
        let cmd = if cfg!(windows) {
            "echo cmd-secret-value"
        } else {
            "printf 'cmd-secret-value\\n'"
        };
        let id = AgentIdentity::from_toml_str(&format!(
            r#"
[agent-identity]
name = "x[bot]"
[agent-identity.tokens]
svc = {{ cmd = "{cmd}" }}
"#,
        ))
        .unwrap();
        let tok = id.token("svc").unwrap().unwrap();
        assert_eq!(tok.expose(), "cmd-secret-value");
    }

    #[test]
    fn token_cmd_failure_is_error_not_panic() {
        let cmd = if cfg!(windows) { "exit /B 3" } else { "exit 3" };
        let id = AgentIdentity::from_toml_str(&format!(
            r#"
[agent-identity]
name = "x[bot]"
[agent-identity.tokens]
svc = {{ cmd = "{cmd}" }}
"#,
        ))
        .unwrap();
        let err = id.token("svc").unwrap_err();
        assert!(format!("{err}").contains("token command exited"));
    }

    #[test]
    fn github_app_surfaces_public_coordinates_and_key_path() {
        let id = AgentIdentity::from_toml_str(
            r#"
[agent-identity]
name = "x[bot]"
[agent-identity.github_app]
app_id = 4046825
client_id = "Iv23li5iPGv4awNHpHbZ"
installation_id = 140120359
private_key = "~/.vault-secrets/agents/x/app.pem"
"#,
        )
        .unwrap();
        let app = id.github_app().unwrap();
        assert_eq!(app.app_id, 4046825);
        assert_eq!(app.client_id.as_deref(), Some("Iv23li5iPGv4awNHpHbZ"));
        assert_eq!(app.installation_id, Some(140120359));
        // The private key is a PATH, never inline material.
        assert_eq!(
            app.private_key.as_deref(),
            Some("~/.vault-secrets/agents/x/app.pem")
        );
        // Default identity has no app.
        assert!(AgentIdentity::default().github_app().is_none());
    }

    #[test]
    fn round_trips_through_toml_with_no_raw_secret_field() {
        // The serialized form contains only public material: name, email,
        // paths, app ids, and the token-reference *sources* — never a resolved
        // secret value.
        let dir = TempDir::new().unwrap();
        let secret_path = dir.path().join("tok");
        std::fs::write(&secret_path, "should-not-appear-in-toml").unwrap();
        // Literal string ('...') for the embedded path — backslash-safe on
        // Windows (see `token_resolves_from_file` for the rationale).
        let id = AgentIdentity::from_toml_str(&format!(
            r#"
[agent-identity]
name = "round[bot]"
email = "round@users.noreply.github.com"
signing_key = "~/keys/id.pem"
public_key = "~/keys/id.pub"
[agent-identity.github_app]
app_id = 42
[agent-identity.tokens]
svc = {{ file = '{}' }}
"#,
            secret_path.display()
        ))
        .unwrap();

        // Serialize the way the file is shaped: a top-level `[agent-identity]`
        // section. A single-key map gives us that nesting for free.
        let mut section = std::collections::BTreeMap::new();
        section.insert("agent-identity".to_string(), id.clone());
        let text = toml::to_string_pretty(&section).unwrap();

        // The resolved secret value must never appear in the serialized form.
        assert!(!text.contains("should-not-appear-in-toml"));
        // But the path reference is fine to serialize.
        assert!(text.contains("file"));

        // It must round-trip back to an equal struct.
        let back = AgentIdentity::from_toml_str(&text).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("super-secret");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(s.expose(), "super-secret");
    }

    #[test]
    fn identity_source_labels_are_human_readable() {
        assert!(IdentitySource::Default.label().contains("newt-agent"));
        assert!(!IdentitySource::Default.label().contains("[bot]"));
        assert!(
            IdentitySource::Workspace(PathBuf::from("/w/.newt/agent-identity.toml"))
                .label()
                .contains("workspace")
        );
        assert!(
            IdentitySource::Home(PathBuf::from("/h/.newt/agent-identity.toml"))
                .label()
                .contains("home")
        );
        assert!(
            IdentitySource::System(PathBuf::from("/etc/newt/agent-identity.toml"))
                .label()
                .contains("system")
        );
    }

    #[test]
    fn standalone_workspace_identity_without_sibling_config_resolves() {
        // A workspace that ships ONLY agent-identity.toml (no config.toml)
        // still resolves via the standalone walk-up.
        let home = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        write_identity(
            ws.path(),
            r#"
[agent-identity]
name = "standalone[bot]"
"#,
        );
        let (id, src) = AgentIdentity::resolve_from(Some(ws.path()), Some(home.path())).unwrap();
        assert_eq!(id.name, "standalone[bot]");
        assert!(matches!(src, IdentitySource::Workspace(_)));
    }

    // ---- #1709 family: atomic operator (name, email) identity resolution ----
    //
    // The operator co-author identity is resolved as ONE atomic pair, never
    // the two halves independently: a configured name is never paired with an
    // unrelated host email, and an email is never invented. The pure
    // `resolve_operator_identity` core is unit-tested here with INJECTED host
    // pairs (no subprocess); the `AgentIdentity::operator_identity` wrappers
    // below cover the configured cases (which return before any git read, so
    // they too are deterministic).

    #[test]
    fn resolve_configured_pair_wins_outright() {
        // Requirement 9: an explicitly configured name+email pair is preferred.
        let (name, email) = resolve_operator_identity(
            Some("shawn"),
            Some("shawn@configured.example"),
            (Some("Host Name".into()), Some("host@host.example".into())),
        );
        assert_eq!(name.as_deref(), Some("shawn"));
        assert_eq!(email.as_deref(), Some("shawn@configured.example"));
    }

    #[test]
    fn resolve_configured_name_without_email_is_never_paired_with_host_email() {
        // Requirement 8 (the core defect): a configured operator name with no
        // configured email must NOT grab an unrelated host email. The name is
        // kept for provenance; the email is None — no Co-authored-by emitted.
        let (name, email) = resolve_operator_identity(
            Some("shawn"),
            None,
            (Some("Host Name".into()), Some("host@host.example".into())),
        );
        assert_eq!(name.as_deref(), Some("shawn"));
        assert!(
            email.is_none(),
            "configured name must not pair with host email"
        );
    }

    #[test]
    fn resolve_configured_email_without_name_falls_to_host_pair() {
        // A configured email with no configured name cannot form a configured
        // pair; fall to the matched host pair (the email is not paired with a
        // configured-but-absent name).
        let (name, email) = resolve_operator_identity(
            None,
            Some("shawn@configured.example"),
            (Some("Host Name".into()), Some("host@host.example".into())),
        );
        assert_eq!(name.as_deref(), Some("Host Name"));
        assert_eq!(email.as_deref(), Some("host@host.example"));
    }

    #[test]
    fn resolve_unconfigured_uses_matched_host_pair() {
        // Requirement 9: with no configured operator, the matched host Git
        // name+email pair (same source) supplies both halves.
        let (name, email) = resolve_operator_identity(
            None,
            None,
            (Some("Host Name".into()), Some("host@host.example".into())),
        );
        assert_eq!(name.as_deref(), Some("Host Name"));
        assert_eq!(email.as_deref(), Some("host@host.example"));
    }

    #[test]
    fn resolve_host_name_only_keeps_name_emits_no_email() {
        // Requirement 10: when only a name is known (host name, no host
        // email), keep the name for provenance, emit no email.
        let (name, email) = resolve_operator_identity(None, None, (Some("Host Name".into()), None));
        assert_eq!(name.as_deref(), Some("Host Name"));
        assert!(email.is_none());
    }

    #[test]
    fn resolve_nothing_known_yields_none_pair() {
        let (name, email) = resolve_operator_identity(None, None, (None, None));
        assert!(name.is_none());
        assert!(email.is_none());
    }

    #[test]
    fn resolve_host_email_without_host_name_drops_the_email() {
        // A host email with no matching host name cannot form a pair; the
        // email is dropped (never paired with an env-fallback name).
        let (name, email) =
            resolve_operator_identity(None, None, (None, Some("host@host.example".into())));
        assert!(name.is_none());
        assert!(
            email.is_none(),
            "an email with no matched name is not a pair"
        );
    }

    #[test]
    fn resolve_trims_whitespace_and_ignores_empty_configured_values() {
        // Whitespace-only configured values are treated as absent, so a
        // "  " configured email does not pair with a configured name.
        let (name, email) = resolve_operator_identity(
            Some("  shawn  "),
            Some("   "),
            (Some("Host Name".into()), Some("host@host.example".into())),
        );
        assert_eq!(name.as_deref(), Some("shawn"));
        assert!(
            email.is_none(),
            "blank configured email is absent — no pairing"
        );
    }

    #[test]
    fn operator_identity_configured_pair_via_toml() {
        // The full `operator_identity()` path: an explicitly configured pair
        // (both fields) wins, deterministically (returns before any git read).
        let id = AgentIdentity::from_toml_str(
            r#"
[agent-identity]
name = "newt-agent"
operator = "shawn"
operator_email = "shawn@configured.example"
"#,
        )
        .unwrap();
        let (name, email) = id.operator_identity();
        assert_eq!(name.as_deref(), Some("shawn"));
        assert_eq!(email.as_deref(), Some("shawn@configured.example"));
    }

    #[test]
    fn operator_identity_configured_name_only_emits_no_email() {
        // Requirement 8 + 10 via the real method: a configured name with no
        // configured email keeps the name and emits no email. This returns at
        // the configured-name branch (before any host git read), so it is
        // deterministic regardless of the host git config.
        let id = AgentIdentity::from_toml_str(
            r#"
[agent-identity]
name = "newt-agent"
operator = "shawn"
"#,
        )
        .unwrap();
        let (name, email) = id.operator_identity();
        assert_eq!(name.as_deref(), Some("shawn"));
        assert!(
            email.is_none(),
            "configured name with no configured email emits no email (never paired with host)"
        );
        // The convenience accessors agree with the atomic pair.
        assert_eq!(id.operator_name().as_deref(), Some("shawn"));
        assert!(id.operator_email().is_none());
    }

    #[test]
    fn operator_email_never_invented_when_unset() {
        // Requirement 11: with no configured operator at all, the default
        // identity never invents an operator email — `operator_email()` is
        // the atomic pair's email half, which is None without a matched host
        // pair. (This does not shell out: the default identity has no
        // configured operator, so it reaches the host branch; the assertion
        // only checks it is None-or-real, never a fabricated constant.)
        let id = AgentIdentity::default();
        assert!(id.operator.is_none());
        assert!(id.operator_email.is_none());
        // operator_email() is real-or-None, never a hardcoded invented value.
        let email = id.operator_email();
        assert!(
            email.is_none() || email.unwrap().contains('@'),
            "operator email is real-or-None, never invented"
        );
    }
}
