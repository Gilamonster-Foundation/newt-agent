//! `newt auth` — the MCP OAuth status table and the interactive flow's driver.
//!
//! Discovers the HTTP-transport MCP servers, reports each one's auth state,
//! and for a named server runs the OAuth 2.1 PKCE flow. The flow itself lives
//! in [`super::mcp_token`] (`run_oauth_flow`, `auth_status`, `OAuthHopPolicy`)
//! and the admission rules in [`super::mcp`]; this is the command driver that
//! resolves config, picks the servers and prints for a human.

use super::*;

fn oauth_policy_from_config(
    config: &newt_core::Config,
    workspace: &std::path::Path,
) -> mcp_token::OAuthHopPolicy {
    let permissions = config.tui.as_ref().map(|tui| &tui.permissions);
    let net = permissions
        .map(|permissions| permissions.to_caveats(&workspace.to_string_lossy()).net)
        .unwrap_or_else(newt_core::Scope::none);
    mcp_token::OAuthHopPolicy::with_explicit_hosts(
        &net,
        permissions
            .map(|permissions| permissions.net.as_slice())
            .unwrap_or_default(),
    )
}

/// Report auth status for every discovered HTTP MCP server, and optionally run
/// the interactive OAuth 2.1 PKCE browser flow for a named server.
///
/// `server_name = None` → print a status table and exit.
/// `server_name = Some(name)` → run the full browser-based flow for `name`.
pub fn run_auth(server_name: Option<&str>) -> anyhow::Result<()> {
    // Discover the HTTP MCP servers from ~/.claude.json and ~/.newt/config.toml.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let resolved = newt_core::Config::resolve()
        .map_err(|error| anyhow::anyhow!("failed to resolve Newt configuration: {error}"))?;
    let oauth_policy = oauth_policy_from_config(&resolved, &workspace);
    let cfg_servers: Vec<newt_core::mcp::McpServerEntry> = resolved.mcp_servers;
    let mcp_toml = newt_core::Config::user_config_dir().map(|d| d.join("mcp.toml"));
    let entries = newt_core::mcp::discover(
        &cfg_servers,
        mcp_toml.as_deref(),
        home.as_deref(),
        &workspace,
    );

    // Collect HTTP-transport servers (the only ones that use OAuth).
    let http_servers: Vec<newt_core::mcp::McpServerEntry> = entries
        .into_iter()
        .filter(|e| e.transport == newt_core::mcp::TransportKind::Http)
        .filter(|e| e.url.is_some())
        .collect();

    match server_name {
        None => {
            // List mode — print a table.
            println!("\nMCP server auth status:\n");
            for entry in &http_servers {
                if let Err(denied) = newt_core::mcp::admit(entry) {
                    println!("  !  {:<30}  not admitted: {denied}", entry.name);
                    continue;
                }
                if mcp::has_plaintext_authorization_header(entry) {
                    println!(
                        "  !  {:<30}  invalid plaintext Authorization; use an environment/file reference",
                        entry.name
                    );
                    continue;
                }
                if mcp::has_configured_authorization_header(entry) {
                    println!("  ✓  {:<30}  configured credential", entry.name);
                    continue;
                }
                let url = entry.url.clone().expect("HTTP entries were URL-filtered");
                let mut statuses = mcp_token::auth_status(&[(entry.name.clone(), url)]);
                let s = statuses.pop().expect("one auth status requested");
                let icon = match s.state {
                    mcp_token::AuthState::Valid => "✓",
                    mcp_token::AuthState::Expired => "↺",
                    mcp_token::AuthState::NeedsFlow => "○",
                    mcp_token::AuthState::NeedsMigration => "!",
                    mcp_token::AuthState::Unregistered => "✗",
                };
                let label = match s.state {
                    mcp_token::AuthState::Valid => "authenticated",
                    mcp_token::AuthState::Expired => "token expired (will refresh on connect)",
                    mcp_token::AuthState::NeedsFlow => "needs login  →  newt auth",
                    mcp_token::AuthState::NeedsMigration => {
                        "legacy/unbound auth state  →  newt auth"
                    }
                    mcp_token::AuthState::Unregistered => "no client registration",
                };
                println!("  {icon}  {:<30}  {label}", s.name);
            }
            println!("\nRun `newt auth <server>` to authenticate a server.");
            Ok(())
        }
        Some(name) => {
            // Flow mode — find the URL and run the browser flow.
            let url = http_servers
                .iter()
                .find(|entry| entry.name == name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Server `{name}` not found in discovered HTTP MCP servers.\n\
                         Run `newt auth` (no argument) to list available servers."
                    )
                })?;
            newt_core::mcp::admit(url).map_err(|denied| {
                anyhow::anyhow!(
                    "Server `{name}` is not admitted for authentication: {denied}. Import/approve it before running OAuth."
                )
            })?;
            if mcp::has_plaintext_authorization_header(url) {
                anyhow::bail!(
                    "Server `{name}` has an invalid plaintext Authorization credential; replace it with an environment/file reference before authenticating"
                );
            }
            if mcp::has_configured_authorization_header(url) {
                anyhow::bail!(
                    "Server `{name}` already has a configured Authorization credential; OAuth is not applicable unless that explicit header is removed"
                );
            }
            let url = url.url.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Server `{name}` not found in discovered HTTP MCP servers.\n\
                         Run `newt auth` (no argument) to list available servers."
                )
            })?;

            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(mcp_token::run_oauth_flow(
                    name,
                    &url,
                    &oauth_policy,
                ))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_policy_reloads_permanent_hosts_in_confined_and_full_access_config() {
        for (preset, grants) in [
            ("workspace_dev", r#"["*", "auth.example.test"]"#),
            ("full_access", r#"["auth.example.test"]"#),
            ("workspace_dev", r#"["auth.example.test"]"#),
        ] {
            let original = format!(
                "# Network approvals\n[tui.permissions]\npreset = \"{preset}\"\nnet = {grants} # Keep this note\n"
            );
            let saved = newt_core::Config::with_net_host(&original, "localhost").unwrap();
            assert!(saved.contains("# Network approvals"));
            assert!(saved.contains("# Keep this note"));
            let config: newt_core::Config = toml::from_str(&saved).unwrap();
            let policy = oauth_policy_from_config(&config, std::path::Path::new("."));
            assert!(policy.permits_host("auth.example.test"));
            assert!(policy.explicitly_grants_host("auth.example.test"));
            assert!(policy.permits_host("localhost"));
            assert!(policy.explicitly_grants_host("localhost"));
            assert!(!policy.explicitly_grants_host("127.0.0.2"));
            assert!(!policy.explicitly_grants_host("*"));
            assert_eq!(
                policy.permits_host("other.example.test"),
                preset == "full_access" || grants.contains("\"*\"")
            );
        }
    }

    #[test]
    fn auth_policy_keeps_default_and_confined_capabilities() {
        let default =
            oauth_policy_from_config(&newt_core::Config::default(), std::path::Path::new("."));
        assert!(!default.permits_host("auth.example.test"));

        let config: newt_core::Config = toml::from_str(
            r#"
                [tui.permissions]
                preset = "workspace_dev"
                net = ["auth.example.test"]
            "#,
        )
        .unwrap();
        let policy = oauth_policy_from_config(&config, std::path::Path::new("."));
        assert!(policy.permits_host("auth.example.test"));
        assert!(policy.explicitly_grants_host("auth.example.test"));
        assert!(!policy.permits_host("other.example.test"));
    }
}
