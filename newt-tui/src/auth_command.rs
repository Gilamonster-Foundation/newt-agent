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

/// The servers `newt auth --all` and `/mcp login-all` sign in to (#3028), in
/// discovery order: admitted HTTP servers with no Authorization header and no
/// credential-reference header of any name (those own their auth) and no
/// usable saved OAuth login.
pub(crate) fn login_all_targets(
    entries: &[newt_core::mcp::McpServerEntry],
    env_lookup: &dyn Fn(&str) -> Option<String>,
    saved_oauth_valid: &dyn Fn(&newt_core::mcp::McpServerEntry) -> bool,
) -> Vec<newt_core::mcp::McpServerEntry> {
    entries
        .iter()
        .filter(|entry| {
            entry.transport == newt_core::mcp::TransportKind::Http
                && entry.url.is_some()
                && !mcp::has_configured_authorization_header(entry)
                && !mcp::has_credential_reference_header(entry)
                && classify_readiness(entry, env_lookup, saved_oauth_valid)
                    == McpReadiness::Configured
        })
        .cloned()
        .collect()
}

/// The load-time auth completeness pass (#3038): one warning line per server
/// [`login_all_targets`] selects that also has no `oauth_client_id`, i.e. no
/// viable auth path at session start. A missing `oauth_client_id` alone is not
/// a gap: `newt auth` registers a client dynamically (RFC 7591) when the
/// authorization server allows it, so a server with a saved login is not
/// warned.
pub(crate) fn missing_auth_warnings(
    entries: &[newt_core::mcp::McpServerEntry],
    env_lookup: &dyn Fn(&str) -> Option<String>,
    saved_oauth_valid: &dyn Fn(&newt_core::mcp::McpServerEntry) -> bool,
) -> Vec<String> {
    login_all_targets(entries, env_lookup, saved_oauth_valid)
        .iter()
        .filter(|entry| entry.oauth_client_id.is_none())
        .map(|entry| {
            let name = &entry.name;
            format!(
                "MCP server `{name}` (http) has no auth: no `headers.Authorization`, no `oauth_client_id`, no saved login; run `newt auth {name}`, or set one of those keys"
            )
        })
        .collect()
}

/// Sign in to each target in turn through `login` (#3028), writing a ✓/✗ line
/// per server and a summary to `out`. `stop` (Ctrl-C) ends the run early;
/// logins that already finished stay saved. An error when any login failed or
/// the run stopped, so `newt auth --all` exits non-zero.
pub(crate) fn login_all(
    targets: &[newt_core::mcp::McpServerEntry],
    mut login: impl FnMut(&newt_core::mcp::McpServerEntry) -> Result<(), String>,
    stop: &dyn Fn() -> bool,
    out: &mut dyn FnMut(&str),
) -> anyhow::Result<()> {
    let (mut attempted, mut failed) = (0, 0);
    for entry in targets {
        if stop() {
            break;
        }
        attempted += 1;
        match login(entry) {
            Ok(()) => out(&format!("  ✓  {}", entry.name)),
            Err(reason) => {
                failed += 1;
                out(&format!("  ✗  {}  {reason}", entry.name));
            }
        }
    }
    let not_attempted = targets.len() - attempted;
    let mut summary = format!(
        "signed in to {} of {} server(s)",
        attempted - failed,
        targets.len()
    );
    if failed > 0 {
        summary.push_str(&format!("; {failed} failed"));
    }
    if not_attempted > 0 {
        summary.push_str(&format!("; stopped, {not_attempted} not attempted"));
    }
    out(&summary);
    anyhow::ensure!(
        failed == 0 && not_attempted == 0,
        "not every MCP login succeeded"
    );
    Ok(())
}

/// The single-server OAuth login `newt auth <server>` runs; `--all` loops it.
fn login_one(
    entry: &newt_core::mcp::McpServerEntry,
    oauth_policy: &mcp_token::OAuthHopPolicy,
) -> anyhow::Result<()> {
    let name = entry.name.as_str();
    newt_core::mcp::admit(entry).map_err(|denied| {
        anyhow::anyhow!(
            "Server `{name}` is not admitted for authentication: {denied}. Import/approve it before running OAuth."
        )
    })?;
    if mcp::has_plaintext_authorization_header(entry) {
        anyhow::bail!(
            "Server `{name}` has an invalid plaintext Authorization credential; replace it with an environment/file reference before authenticating"
        );
    }
    if mcp::has_configured_authorization_header(entry) {
        anyhow::bail!(
            "Server `{name}` already has a configured Authorization credential; OAuth is not applicable unless that explicit header is removed"
        );
    }
    let configured_oauth = mcp_token::ConfiguredOAuth::from_entry(entry);
    let url = entry.url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Server `{name}` not found in discovered HTTP MCP servers.\n\
                 Run `newt auth` (no argument) to list available servers."
        )
    })?;
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(mcp_token::run_oauth_flow(
            name,
            &url,
            &configured_oauth,
            oauth_policy,
        ))
    })
}

/// Report auth status for every discovered HTTP MCP server, and optionally run
/// the interactive OAuth 2.1 PKCE browser flow for a named server.
///
/// `server_name = None` → print a status table and exit.
/// `server_name = Some(name)` → run the full browser-based flow for `name`.
/// `all` → run that flow for every server [`login_all_targets`] selects.
pub fn run_auth(server_name: Option<&str>, all: bool) -> anyhow::Result<()> {
    // Discover the HTTP MCP servers from ~/.claude.json and ~/.newt/config.toml.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let resolved = crate::migration_notices::read(|report| newt_core::Config::resolve(report))
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

    if all {
        let targets = login_all_targets(
            &http_servers,
            &|v| std::env::var(v).ok(),
            &crate::mcp::saved_oauth_valid,
        );
        // Ctrl-C ends the process (newt auth installs no SIGINT handler);
        // each finished login is already saved.
        return login_all(
            &targets,
            |entry| login_one(entry, &oauth_policy).map_err(|error| format!("{error:#}")),
            &|| false,
            &mut |line| println!("{line}"),
        );
    }
    match server_name {
        None => {
            // List mode — print a table.
            println!("\nMCP server auth status:\n");
            for entry in &http_servers {
                match classify_readiness(
                    entry,
                    &|v| std::env::var(v).ok(),
                    &crate::mcp::saved_oauth_valid,
                ) {
                    McpReadiness::NotAdmitted => {
                        let denied = newt_core::mcp::admit(entry)
                            .expect_err("classified NotAdmitted implies admission denied");
                        println!("  !  {:<30}  not admitted: {denied}", entry.name);
                        continue;
                    }
                    McpReadiness::Authenticated => {
                        println!("  ✓  {:<30}  configured credential", entry.name);
                        continue;
                    }
                    McpReadiness::Configured => {}
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
            println!(
                "\nRun `newt auth <server>` to authenticate a server, or `newt auth --all` for every server that needs it."
            );
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
            login_one(url, &oauth_policy)
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

    fn http(name: &str) -> newt_core::mcp::McpServerEntry {
        newt_core::mcp::McpServerEntry {
            enabled: true,
            name: name.into(),
            transport: newt_core::mcp::TransportKind::Http,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: Some(format!("https://{name}.example.test/mcp")),
            headers: std::collections::BTreeMap::new(),
            login_argv: Vec::new(),
            request_timeout_secs: None,
            oauth_client_id: None,
            oauth_scopes: None,
            oauth_callback_port: None,
            trust: newt_core::mcp::McpTrust::Trusted,
            origin: None,
        }
    }

    /// #3028: only admitted HTTP servers with no configured header/env
    /// credential and no valid saved login are signed in, in discovery order.
    #[test]
    fn login_all_targets_only_the_servers_that_need_a_login_3028() {
        let mut env_header = http("env-header");
        env_header.headers.insert(
            "Authorization".into(),
            newt_core::mcp::SecretValue::Literal("Bearer ${TOKEN}".into()),
        );
        let mut disabled = http("disabled");
        disabled.enabled = false;
        let mut stdio = http("stdio");
        stdio.transport = newt_core::mcp::TransportKind::Stdio;
        stdio.url = None;
        let entries = vec![
            http("needs-a"),
            http("signed-in"),
            env_header,
            disabled,
            stdio,
            http("needs-b"),
        ];
        let targets = login_all_targets(
            &entries,
            &|var| (var == "TOKEN").then(|| "secret".to_string()),
            &|entry| entry.name == "signed-in",
        );
        let names: Vec<&str> = targets.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["needs-a", "needs-b"]);
    }

    /// #3038: an HTTP server with no viable auth path is warned at load,
    /// naming the missing keys and the fix. An Authorization header, a
    /// credential-reference header of any name, or `oauth_client_id` is an
    /// auth configuration; a saved (dynamically registered) login is a viable
    /// path even without `oauth_client_id`.
    #[test]
    fn missing_auth_warnings_name_only_servers_with_no_auth_path_3038() {
        use newt_core::agent_identity::SecretRef;
        use newt_core::mcp::SecretValue;
        let mut plaintext = http("plaintext");
        plaintext.headers.insert(
            "Authorization".into(),
            SecretValue::Literal("Bearer abc".into()),
        );
        let mut api_key = http("api-key");
        api_key.headers.insert(
            "X-API-Key".into(),
            SecretValue::Ref(SecretRef {
                env: None,
                file: Some("key.txt".into()),
                cmd: None,
            }),
        );
        let mut client_id = http("client-id");
        client_id.oauth_client_id = Some("newt-public".into());
        let mut stdio = http("stdio");
        stdio.transport = newt_core::mcp::TransportKind::Stdio;
        stdio.url = None;
        let entries = vec![
            http("bare"),
            plaintext,
            api_key,
            client_id,
            http("dcr-login"),
            stdio,
        ];
        let warnings =
            missing_auth_warnings(&entries, &|_| None, &|entry| entry.name == "dcr-login");
        assert_eq!(
            warnings,
            ["MCP server `bare` (http) has no auth: no `headers.Authorization`, no `oauth_client_id`, no saved login; run `newt auth bare`, or set one of those keys"]
        );
    }

    /// #3028: every target is attempted in order, each gets a ✓/✗ line, the
    /// summary counts them, and any failure makes the run an error (non-zero
    /// exit for `newt auth --all`).
    #[test]
    fn login_all_reports_each_server_and_fails_when_any_login_fails_3028() {
        let targets = vec![http("a"), http("b"), http("c")];
        let mut attempted = Vec::new();
        let mut out = Vec::new();
        let result = login_all(
            &targets,
            |entry| {
                attempted.push(entry.name.clone());
                if entry.name == "b" {
                    Err("HTTP 400".into())
                } else {
                    Ok(())
                }
            },
            &|| false,
            &mut |line| out.push(line.to_string()),
        );
        assert!(result.is_err(), "one failed login must fail the run");
        assert_eq!(attempted, ["a", "b", "c"]);
        assert_eq!(
            out,
            [
                "  ✓  a",
                "  ✗  b  HTTP 400",
                "  ✓  c",
                "signed in to 2 of 3 server(s); 1 failed",
            ]
        );

        let mut out = Vec::new();
        assert!(login_all(&targets, |_| Ok(()), &|| false, &mut |l| out
            .push(l.to_string()))
        .is_ok());
        assert_eq!(out.last().unwrap(), "signed in to 3 of 3 server(s)");
    }

    /// #3028: Ctrl-C stops the run; servers already signed in stay signed in
    /// and the rest are reported as not attempted.
    #[test]
    fn login_all_stops_on_interrupt_3028() {
        let targets = vec![http("a"), http("b"), http("c")];
        let stopped = std::cell::Cell::new(false);
        let mut attempted = Vec::new();
        let mut out = Vec::new();
        let result = login_all(
            &targets,
            |entry| {
                attempted.push(entry.name.clone());
                stopped.set(true);
                Ok(())
            },
            &|| stopped.get(),
            &mut |line| out.push(line.to_string()),
        );
        assert!(result.is_err());
        assert_eq!(attempted, ["a"]);
        assert_eq!(
            out.last().unwrap(),
            "signed in to 1 of 3 server(s); stopped, 2 not attempted"
        );
    }

    /// #3028 review: a server authenticated by a non-`Authorization`
    /// credential reference (`X-API-Key = { env = ... }`, a `file` ref, or a
    /// `${VAR}` literal) is not sent to browser OAuth; an ordinary protocol
    /// header is not a credential and does not exempt a server.
    #[test]
    fn login_all_skips_servers_with_any_credential_reference_header_3028() {
        use newt_core::agent_identity::SecretRef;
        use newt_core::mcp::SecretValue;
        let with_header = |name: &str, header: &str, value: SecretValue| {
            let mut entry = http(name);
            entry.headers.insert(header.into(), value);
            entry
        };
        let entries = vec![
            with_header(
                "env-key",
                "X-API-Key",
                SecretValue::Ref(SecretRef {
                    env: Some("MCP_REVIEW_KEY".into()),
                    file: None,
                    cmd: None,
                }),
            ),
            with_header(
                "file-key",
                "X-API-Key",
                SecretValue::Ref(SecretRef {
                    env: None,
                    file: Some("key.txt".into()),
                    cmd: None,
                }),
            ),
            with_header(
                "literal-ref",
                "X-Token",
                SecretValue::Literal("${MCP_REVIEW_KEY}".into()),
            ),
            with_header(
                "protocol-header",
                "Accept",
                SecretValue::Literal("application/json".into()),
            ),
        ];
        let targets = login_all_targets(
            &entries,
            &|var| (var == "MCP_REVIEW_KEY").then(|| "k".to_string()),
            &|_| false,
        );
        let names: Vec<&str> = targets.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["protocol-header"]);
        // The shared classifier agrees: an env credential header is a
        // configured credential whatever its name.
        assert_eq!(
            classify_readiness(&entries[0], &|_| Some("k".into()), &|_| false),
            McpReadiness::Authenticated
        );
    }
}
