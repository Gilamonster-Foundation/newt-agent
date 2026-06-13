//! `newt identity` — print the resolved agent commit identity.
//!
//! Prints the resolved [`newt_core::AgentIdentity`]: the git author name +
//! email, which layer it resolved from, the signing-key path **and its
//! fingerprint** (when the key loads), the GitHub App's public coordinates,
//! and the configured token *names*.
//!
//! It NEVER prints a secret value — not a private key, not a token. The
//! signing key is loaded only to derive its public BLAKE3 fingerprint (the §6
//! writer fingerprint / mesh `AgentKey` root); the private material never
//! leaves [`newt_identity`].

use std::path::Path;

use newt_core::AgentIdentity;

/// Run `newt identity`.
///
/// `config_path` is accepted for signature parity with the other subcommands
/// (and so a future `--config`-scoped identity file can hang off it); the
/// identity itself resolves via [`AgentIdentity::resolve_with_source`].
pub fn run(_config_path: Option<&Path>) -> anyhow::Result<()> {
    let (identity, source) = AgentIdentity::resolve_with_source()?;
    print_identity(&identity, &source.label(), &mut std::io::stdout())?;
    Ok(())
}

/// Render the resolved identity to `out`. Split from [`run`] so it is unit
/// testable against an in-memory buffer.
fn print_identity(
    identity: &AgentIdentity,
    source_label: &str,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let (name, email) = identity.git_author();

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

    fn render(identity: &AgentIdentity, source: &str) -> String {
        let mut buf = Vec::new();
        print_identity(identity, source, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn prints_default_identity_no_secrets() {
        let out = render(
            &AgentIdentity::default(),
            "compiled-in default (newt-agent[bot])",
        );
        assert!(out.contains("newt-agent[bot]"));
        assert!(out.contains("293447090+newt-agent[bot]@users.noreply.github.com"));
        assert!(out.contains("Co-Authored-By:"));
        assert!(out.contains("signing-key: <none>"));
        assert!(out.contains("github-app:  <none>"));
        assert!(out.contains("tokens:      <none>"));
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
        let out = render(&id, "home (~/.newt/agent-identity.toml)");
        unsafe { std::env::remove_var("NEWT_IDENTITY_TEST_TOKEN") };
        // The NAME is shown...
        assert!(out.contains("service_a"));
        assert!(out.contains("names only"));
        // ...but the resolved VALUE is never printed.
        assert!(!out.contains("do-not-print-me"));
    }

    #[test]
    fn prints_github_app_coordinates_but_no_secret() {
        let id = AgentIdentity {
            github_app: Some(GithubApp {
                app_id: 4046825,
                client_id: Some("Iv23li5iPGv4awNHpHbZ".to_string()),
                installation_id: Some(140120359),
                private_key: Some("~/.vault-secrets/agents/newt-agent/app.pem".to_string()),
            }),
            ..AgentIdentity::default()
        };
        let out = render(&id, "workspace (.newt/agent-identity.toml)");
        assert!(out.contains("app_id:          4046825"));
        assert!(out.contains("installation_id: 140120359"));
        // The private key shows only as a PATH, marked as such.
        assert!(out.contains("app.pem (path)"));
    }

    #[test]
    fn missing_signing_key_renders_unavailable_not_panic() {
        let id = AgentIdentity {
            signing_key: Some("/nonexistent/vault/path/identity.pem".to_string()),
            ..AgentIdentity::default()
        };
        let out = render(&id, "home");
        assert!(out.contains("/nonexistent/vault/path/identity.pem"));
        assert!(out.contains("<unavailable"));
    }
}
