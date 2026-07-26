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

    /// Optional model identifier used in the Co-authored-by header.
    /// When set, the trailer reads `Co-Authored-By: name (model) <email>`.
    /// When unset, the header omits the model parenthetical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

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
            signing_key: None,
            public_key: None,
            github_app: None,
            tokens: std::collections::BTreeMap::new(),
        }
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
                let candidate = cfg.with_file_name("agent-identity.toml");
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
            let candidate = dir.join("agent-identity.toml");
            if candidate.is_file() {
                return Ok((Self::load(&candidate)?, IdentitySource::Home(candidate)));
            }
        }

        // 3. System.
        let system = PathBuf::from("/etc/newt/agent-identity.toml");
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

    /// A `Co-Authored-By:` trailer line for commit messages.
    ///
    /// When `model` is `Some`, the header reads `Co-Authored-By: name (model)
    /// <email>`; otherwise the header is `Co-Authored-By: name <email>`
    /// (no model parenthetical).
    #[must_use]
    pub fn co_author_trailer(&self) -> String {
        match &self.model {
            Some(model) => format!("Co-Authored-By: {} ({}) <{}>", self.name, model, self.email),
            None => format!("Co-Authored-By: {} <{}>", self.name, self.email),
        }
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
        let candidate = current.join(".newt").join("agent-identity.toml");
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
        let path = newt.join("agent-identity.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        path
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
    fn co_author_trailer_format() {
        let id = AgentIdentity::default();
        assert_eq!(
            id.co_author_trailer(),
            "Co-Authored-By: newt-agent <309460085+newt-agent@users.noreply.github.com>"
        );
    }

    #[test]
    fn co_author_trailer_with_model() {
        let id = AgentIdentity {
            model: Some("sakamakismile/Ornith-1.0-35B-NVFP4".to_string()),
            ..AgentIdentity::default()
        };
        assert_eq!(
            id.co_author_trailer(),
            "Co-Authored-By: newt-agent (sakamakismile/Ornith-1.0-35B-NVFP4) <309460085+newt-agent@users.noreply.github.com>"
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
}
