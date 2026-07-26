//! `newt identity` — show or set the agent commit identity.
//!
//! Prints the resolved [`newt_core::AgentIdentity`]: the git author name +
//! email, which layer it resolved from, the signing-key path **and its
//! fingerprint** (when the key loads), the GitHub App's public coordinates,
//! and the configured token *names*.
//!
//! `newt identity set --name … --email …` writes `~/.newt/agent-identity.toml`
//! (or a workspace `.newt/agent-identity.toml` with `--workspace`) via
//! [`AgentIdentity::save`] — the same write seam a future setup dialog will
//! use. It NEVER prints a secret value.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use newt_core::{AgentIdentity, IdentitySource, AGENT_IDENTITY_FILENAME};

/// Subcommands under `newt identity`. Bare `newt identity` shows the resolved
/// identity; `set` writes an override file.
#[derive(Debug, Clone, Subcommand)]
pub enum IdentityCmd {
    /// Write name + email to an `agent-identity.toml` override.
    ///
    /// Default target is the user config (`~/.newt/agent-identity.toml`).
    /// Pass `--workspace` to write `.newt/agent-identity.toml` in the cwd
    /// instead. This is the write path a future setup dialog will call.
    Set {
        /// Git author/committer name (any string you want attributed).
        #[arg(long)]
        name: String,
        /// Git author/committer email (any address; GitHub User no-reply form
        /// links commits to that account).
        #[arg(long)]
        email: String,
        /// Write to `./.newt/agent-identity.toml` instead of the user config.
        #[arg(long)]
        workspace: bool,
    },
}

/// Run `newt identity` (show) or `newt identity set …`.
///
/// `config_path` is accepted for signature parity with the other subcommands
/// (and so a future `--config`-scoped identity file can hang off it); the
/// show path resolves via [`AgentIdentity::resolve_with_source`].
pub fn run(config_path: Option<&Path>, cmd: Option<IdentityCmd>) -> anyhow::Result<()> {
    match cmd {
        None => show(config_path),
        Some(IdentityCmd::Set {
            name,
            email,
            workspace,
        }) => set(name, email, workspace),
    }
}

fn show(_config_path: Option<&Path>) -> anyhow::Result<()> {
    let (identity, source) = AgentIdentity::resolve_with_source()?;
    print_identity(&identity, &source, &mut std::io::stdout())?;
    Ok(())
}

fn set(name: String, email: String, workspace: bool) -> anyhow::Result<()> {
    let path = if workspace {
        workspace_identity_path()?
    } else {
        AgentIdentity::user_identity_path().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve user config dir (~/.newt); set HOME or NEWT_CONFIG_DIR")
        })?
    };
    let identity = AgentIdentity {
        name: name.clone(),
        email: email.clone(),
        ..AgentIdentity::default()
    };
    identity.save(&path)?;
    println!("Wrote agent identity to {}", path.display());
    println!("  name:  {name}");
    println!("  email: {email}");
    println!("Verify with: newt identity");
    Ok(())
}

fn workspace_identity_path() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(cwd.join(".newt").join(AGENT_IDENTITY_FILENAME))
}

/// Render the resolved identity to `out`. Split from [`show`] so it is unit
/// testable against an in-memory buffer.
fn print_identity(
    identity: &AgentIdentity,
    source: &IdentitySource,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let (name, email) = identity.git_author();
    let source_label = source.label();

    writeln!(out, "Agent commit identity")?;
    writeln!(out, "  name:    {name}")?;
    writeln!(out, "  email:   {email}")?;
    writeln!(out, "  source:  {source_label}")?;
    writeln!(out, "  trailer: {}", identity.co_author_trailer())?;

    // Signing key: print the PATH and, if it loads, the public fingerprint.
    // The private material never leaves newt-identity; we surface only the
    // BLAKE3 fingerprint of the public half.
    match identity.signing_key_path() {
        Some(path) => {
            writeln!(out, "  signing-key: {}", path.display())?;
            match load_fingerprint(&path) {
                Ok(fp) => writeln!(out, "    fingerprint: {fp}")?,
                Err(e) => writeln!(out, "    fingerprint: <unavailable: {e}>")?,
            }
        }
        None => writeln!(out, "  signing-key: <none>")?,
    }

    if let Some(path) = identity.public_key_path() {
        writeln!(out, "  public-key:  {}", path.display())?;
    }

    // GitHub App: public coordinates + the private-key PATH only. No token is
    // minted (out of scope) and no secret is printed.
    match identity.github_app() {
        Some(app) => {
            writeln!(out, "  github-app:")?;
            writeln!(out, "    app_id:          {}", app.app_id)?;
            if let Some(cid) = &app.client_id {
                writeln!(out, "    client_id:       {cid}")?;
            }
            if let Some(inst) = app.installation_id {
                writeln!(out, "    installation_id: {inst}")?;
            }
            if let Some(pk) = &app.private_key {
                writeln!(out, "    private_key:     {pk} (path)")?;
            }
        }
        None => writeln!(out, "  github-app:  <none>")?,
    }

    // Token NAMES only — never the resolved values.
    if identity.tokens.is_empty() {
        writeln!(out, "  tokens:      <none>")?;
    } else {
        let names: Vec<&str> = identity.tokens.keys().map(String::as_str).collect();
        writeln!(out, "  tokens:      {} (names only)", names.join(", "))?;
    }

    if let Some(hint) = source.configure_hint() {
        writeln!(out)?;
        writeln!(out, "{hint}")?;
    }

    Ok(())
}

/// Load the signing key at `path` and return its public fingerprint.
///
/// **Load-only**: an explicitly-configured signing-key path that doesn't exist
/// is an error, NOT a silent mint. newt must never create a key at a vault
/// path the operator named. (The `load_or_generate` mint path is reserved for
/// the implicit `~/.newt/identity.pem` operator key, not for a configured
/// identity key.)
fn load_fingerprint(path: &Path) -> anyhow::Result<String> {
    let key = newt_identity::load_user_key(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(key.fingerprint().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{AgentIdentity, GithubApp, SecretRef};

    fn render(identity: &AgentIdentity, source: &IdentitySource) -> String {
        let mut buf = Vec::new();
        print_identity(identity, source, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn prints_default_identity_no_secrets() {
        let out = render(&AgentIdentity::default(), &IdentitySource::Default);
        assert!(out.contains("newt-agent"));
        assert!(out.contains("309460085+newt-agent@users.noreply.github.com"));
        assert!(!out.contains("[bot]"));
        assert!(out.contains("Co-Authored-By:"));
        assert!(out.contains("signing-key: <none>"));
        assert!(out.contains("github-app:  <none>"));
        assert!(out.contains("tokens:      <none>"));
        assert!(
            out.contains("newt identity set"),
            "default source must hint how to override: {out}"
        );
    }

    #[test]
    fn prints_token_names_never_values() {
        // SAFETY: single-threaded test with a unique var name.
        unsafe { std::env::set_var("NEWT_IDENTITY_TEST_TOKEN", "do-not-print-me") };
        let mut tokens = std::collections::BTreeMap::new();
        tokens.insert(
            "service_a".to_string(),
            SecretRef {
                env: Some("NEWT_IDENTITY_TEST_TOKEN".to_string()),
                ..SecretRef::default()
            },
        );
        let id = AgentIdentity {
            tokens,
            ..AgentIdentity::default()
        };
        let out = render(
            &id,
            &IdentitySource::Home(PathBuf::from("/h/.newt/agent-identity.toml")),
        );
        assert!(out.contains("service_a"));
        assert!(out.contains("names only"));
        assert!(!out.contains("do-not-print-me"));
        // Configured source: no "how to override" hint.
        assert!(!out.contains("newt identity set"));
    }

    #[test]
    fn prints_github_app_public_coords() {
        let id = AgentIdentity {
            github_app: Some(GithubApp {
                app_id: 1,
                client_id: Some("Iv23liTEST".into()),
                installation_id: Some(99),
                private_key: Some("~/.newt/app.pem".into()),
            }),
            ..AgentIdentity::default()
        };
        let out = render(
            &id,
            &IdentitySource::Workspace(PathBuf::from("/w/.newt/agent-identity.toml")),
        );
        assert!(out.contains("app_id:          1"));
        assert!(out.contains("client_id:       Iv23liTEST"));
        assert!(out.contains("installation_id: 99"));
        assert!(out.contains("private_key:     ~/.newt/app.pem (path)"));
    }
}
